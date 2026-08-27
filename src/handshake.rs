//! Drives the M2 handshake over a live QUIC connection: opens/accepts the
//! `control` stream, exchanges `ClientHello`/`ServerHello`, computes the
//! channel-binding signature (spec 2.3) using the real TLS exporter, and
//! exchanges `AuthPubkey`/`AuthResult`. Advances a [`ConnectionSm`] on each
//! side from `Handshaking` through `Authenticating` to `Authenticated`.
//!
//! Out of scope for M2 (per the PoC brief): `AuthChallengeRenew`-based
//! retry, challenge-reuse tracking, and `AuthPasskeyAssertion`. A failed
//! signature check ends the handshake in one shot rather than looping.

use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::Rng;

use crate::connection_sm::{ConnectionSm, defaults as timeouts};
use crate::messages::{
    self, AuthMethod, AuthPubkey, AuthResult, AuthStatus, ClientHello, ServerHello,
};
use crate::reason_code::ReasonCode;
use crate::stream_kind::StreamKind;
use crate::stream_reader::{EnvelopeReader, StreamReadError, write_envelope};
use crate::{auth, permission_set, prologue};

/// The `control` stream, kept alive past the handshake (spec 4.2: persistent
/// for the whole session) so later exchanges -- TimeSync, KeepAlive, ...
/// -- can keep using the same stream and its already-buffered bytes.
pub struct ControlChannel {
    pub send: quinn::SendStream,
    pub reader: EnvelopeReader,
}

/// Everything that can go wrong while driving the M2 handshake.
#[derive(Debug)]
pub enum HandshakeError {
    Quic(quinn::ConnectionError),
    Read(StreamReadError),
    Write(quinn::WriteError),
    Decode(ciborium::de::Error<std::io::Error>),
    /// A message arrived that spec 4.1 does not permit in the current
    /// Connection SM state.
    ProtocolViolation(ReasonCode),
    Auth(auth::AuthError),
    /// The peer's `AuthResult.status` was not `Ok`.
    AuthDenied,
    /// `HANDSHAKE_TIMEOUT` (10s, spec 4.7) elapsed before ServerHello was
    /// exchanged.
    HandshakeTimeout,
    /// `AUTH_TIMEOUT` (60s, spec 4.7) elapsed before AuthResult arrived.
    AuthTimeout,
}

impl From<StreamReadError> for HandshakeError {
    fn from(e: StreamReadError) -> Self {
        Self::Read(e)
    }
}

impl From<quinn::WriteError> for HandshakeError {
    fn from(e: quinn::WriteError) -> Self {
        Self::Write(e)
    }
}

impl HandshakeError {
    /// The `ReasonCode` a caller should close the connection with, per
    /// spec 4.1/4.7/4.8's table (used by `sardp-server`/`sardp-client`'s
    /// graceful-close path; library callers that just need pass/fail can
    /// ignore this).
    pub fn reason_code(&self) -> ReasonCode {
        match self {
            Self::HandshakeTimeout => ReasonCode::TRANSPORT_HANDSHAKE_TIMEOUT,
            Self::AuthTimeout => ReasonCode::AUTH_TIMEOUT,
            Self::ProtocolViolation(reason) => *reason,
            Self::AuthDenied => ReasonCode::AUTH_SIGNATURE_INVALID,
            Self::Auth(_) => ReasonCode::AUTH_SIGNATURE_INVALID,
            Self::Quic(_) | Self::Read(_) | Self::Write(_) | Self::Decode(_) => {
                ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE
            }
        }
    }
}

/// Outcome of a successful M2 handshake. The resulting [`ConnectionSm`]
/// (state `Authenticated`) and the still-open `control` stream are
/// returned alongside this so the caller can keep driving the session
/// (spec 4.2: `control` is persistent for the whole session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeOutcome {
    pub session_id: [u8; 16],
    pub reconnect_token: [u8; 32],
    pub granted_permissions: u32,
}

fn check(sm: &ConnectionSm, type_id: u16) -> Result<(), HandshakeError> {
    sm.check_message(type_id)
        .map_err(|v| HandshakeError::ProtocolViolation(v.reason))
}

/// Drives the client side with the spec 4.7 default timeouts
/// (`HANDSHAKE_TIMEOUT`=10s, `AUTH_TIMEOUT`=60s). See
/// [`client_handshake_with_timeouts`] to override them (e.g. a
/// LAN-only deployment choosing tighter bounds, or a test proving the
/// timeout mechanism itself fires without a real multi-second wait).
pub async fn client_handshake(
    connection: &quinn::Connection,
    signing_key: &SigningKey,
    client_name: &str,
    user_id: &str,
    device_id: &str,
) -> Result<(HandshakeOutcome, ConnectionSm, ControlChannel), HandshakeError> {
    client_handshake_with_timeouts(
        connection,
        signing_key,
        client_name,
        user_id,
        device_id,
        timeouts::HANDSHAKE_TIMEOUT,
        timeouts::AUTH_TIMEOUT,
    )
    .await
}

/// Drives the client side: opens the `control` stream, sends
/// `ClientHello`, signs the channel-binding exporter with `signing_key`,
/// and waits for `AuthResult`. `handshake_timeout`/`auth_timeout` bound
/// the two phases (spec 4.7).
#[allow(clippy::too_many_arguments)]
pub async fn client_handshake_with_timeouts(
    connection: &quinn::Connection,
    signing_key: &SigningKey,
    client_name: &str,
    user_id: &str,
    device_id: &str,
    handshake_timeout: std::time::Duration,
    auth_timeout: std::time::Duration,
) -> Result<(HandshakeOutcome, ConnectionSm, ControlChannel), HandshakeError> {
    let mut sm = ConnectionSm::new();
    let (mut send, recv) = connection.open_bi().await.map_err(HandshakeError::Quic)?;
    let mut reader = EnvelopeReader::new(recv);

    // Spec 4.7 HANDSHAKE_TIMEOUT (10s): bounds the ClientHello/ServerHello
    // exchange. `sm`/`send`/`reader` are borrowed, not moved, so they're
    // still usable below once this block (and its single `.await` point)
    // completes.
    let (client_hello_bytes, server_hello, server_hello_bytes) =
        tokio::time::timeout(handshake_timeout, async {
            let mut prologue_bytes = Vec::new();
            prologue::encode(StreamKind::Control, 1, 0, &mut prologue_bytes);
            send.write_all(&prologue_bytes).await?;

            let client_hello = ClientHello {
                client_name: client_name.to_string(),
                client_version: env!("CARGO_PKG_VERSION").to_string(),
                capabilities: vec![],
                auth_methods: vec![AuthMethod::PublicKey],
            };
            let client_hello_bytes = messages::encode(&client_hello);
            check(&sm, messages::type_id::CLIENT_HELLO)?;
            write_envelope(
                &mut send,
                messages::type_id::CLIENT_HELLO,
                &client_hello_bytes,
            )
            .await?;

            let (type_raw, payload) = reader
                .read_envelope(StreamKind::Control.max_envelope_length())
                .await?;
            check(&sm, type_raw)?;
            let server_hello: ServerHello =
                messages::decode(&payload).map_err(HandshakeError::Decode)?;
            let server_hello_bytes = payload;

            Ok::<_, HandshakeError>((client_hello_bytes, server_hello, server_hello_bytes))
        })
        .await
        .map_err(|_elapsed| HandshakeError::HandshakeTimeout)??;

    sm.complete_handshake()
        .map_err(|v| HandshakeError::ProtocolViolation(v.reason))?;

    // Spec 4.7 AUTH_TIMEOUT (60s): bounds the AuthPubkey/AuthResult round
    // trip. This PoC does not implement the `AuthChallengeRenew` retry
    // loop (documented M2 out-of-scope item), so "resets per attempt"
    // degenerates to bounding this single attempt.
    let auth_result: AuthResult = tokio::time::timeout(auth_timeout, async {
        let context = auth::channel_binding_context(
            &client_hello_bytes,
            &server_hello_bytes,
            &server_hello.auth_challenge,
        );
        let exporter =
            auth::compute_exporter(connection, &context).map_err(HandshakeError::Auth)?;
        let signature = auth::sign_exporter(signing_key, &exporter);

        let auth_pubkey = AuthPubkey {
            user_id: user_id.to_string(),
            device_id: device_id.to_string(),
            public_key: signing_key.verifying_key().as_bytes().to_vec(),
            signature,
        };
        let auth_pubkey_bytes = messages::encode(&auth_pubkey);
        check(&sm, messages::type_id::AUTH_PUBKEY)?;
        write_envelope(
            &mut send,
            messages::type_id::AUTH_PUBKEY,
            &auth_pubkey_bytes,
        )
        .await?;

        // `AuthResult` is the message that *exits* Authenticating (to
        // Authenticated on success, or to Closing on denial); spec 4.1's
        // Authenticating row lists only the messages that keep the
        // connection *in* that state (AuthPubkey, retries), not this
        // transition itself. So its legality is enforced by
        // `complete_authentication()`/`record_auth_failure()` below, not
        // by `check()`.
        let (_type_raw, payload) = reader
            .read_envelope(StreamKind::Control.max_envelope_length())
            .await?;
        let auth_result: AuthResult = messages::decode(&payload).map_err(HandshakeError::Decode)?;
        Ok::<_, HandshakeError>(auth_result)
    })
    .await
    .map_err(|_elapsed| HandshakeError::AuthTimeout)??;

    match auth_result.status {
        AuthStatus::Ok => {
            sm.complete_authentication()
                .map_err(|v| HandshakeError::ProtocolViolation(v.reason))?;
            let outcome = HandshakeOutcome {
                session_id: auth_result.session_id,
                reconnect_token: auth_result.reconnect_token,
                granted_permissions: auth_result.granted_permissions,
            };
            Ok((outcome, sm, ControlChannel { send, reader }))
        }
        AuthStatus::MfaRequired | AuthStatus::Denied => Err(HandshakeError::AuthDenied),
    }
}

/// Drives the server side with the spec 4.7 default timeouts. See
/// [`server_handshake_with_timeouts`] to override them.
pub async fn server_handshake(
    connection: &quinn::Connection,
    server_name: &str,
    trusted_public_key: &VerifyingKey,
) -> Result<(HandshakeOutcome, ConnectionSm, ControlChannel), HandshakeError> {
    server_handshake_with_timeouts(
        connection,
        server_name,
        trusted_public_key,
        timeouts::HANDSHAKE_TIMEOUT,
        timeouts::AUTH_TIMEOUT,
    )
    .await
}

/// Drives the server side: accepts the `control` stream, sends
/// `ServerHello` with a fresh challenge, verifies the client's
/// `AuthPubkey` signature against `trusted_public_key` using the real TLS
/// exporter, and sends `AuthResult`. `handshake_timeout`/`auth_timeout`
/// bound the two phases (spec 4.7).
pub async fn server_handshake_with_timeouts(
    connection: &quinn::Connection,
    server_name: &str,
    trusted_public_key: &VerifyingKey,
    handshake_timeout: std::time::Duration,
    auth_timeout: std::time::Duration,
) -> Result<(HandshakeOutcome, ConnectionSm, ControlChannel), HandshakeError> {
    let mut sm = ConnectionSm::new();
    let (mut send, recv) = connection.accept_bi().await.map_err(HandshakeError::Quic)?;
    let mut reader = EnvelopeReader::new(recv);

    // Spec 4.7 HANDSHAKE_TIMEOUT (10s): bounds the ClientHello/ServerHello
    // exchange, same phase boundary as the client side.
    let (client_hello_bytes, server_hello_bytes, auth_challenge) =
        tokio::time::timeout(handshake_timeout, async {
            let stream_prologue = reader.read_prologue().await?;
            if stream_prologue.kind != StreamKind::Control {
                // Spec 2.2.1: "未認証状態でcontrol以外のストリームが開かれたら即切断".
                return Err(HandshakeError::ProtocolViolation(
                    ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
                ));
            }

            let (type_raw, payload) = reader
                .read_envelope(StreamKind::Control.max_envelope_length())
                .await?;
            check(&sm, type_raw)?;
            let client_hello: ClientHello =
                messages::decode(&payload).map_err(HandshakeError::Decode)?;
            let client_hello_bytes = payload;
            let _ = client_hello; // only its bytes are needed for the channel binding

            let mut auth_challenge = [0u8; 32];
            rand::rng().fill_bytes(&mut auth_challenge);

            let server_hello = ServerHello {
                server_name: server_name.to_string(),
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                capabilities: vec![],
                auth_policy: messages::AuthPolicy {
                    accepted_combinations: vec![messages::AuthCombination {
                        methods: vec![AuthMethod::PublicKey],
                        priority: 0,
                    }],
                },
                auth_challenge,
            };
            let server_hello_bytes = messages::encode(&server_hello);
            check(&sm, messages::type_id::SERVER_HELLO)?;
            write_envelope(
                &mut send,
                messages::type_id::SERVER_HELLO,
                &server_hello_bytes,
            )
            .await?;

            Ok::<_, HandshakeError>((client_hello_bytes, server_hello_bytes, auth_challenge))
        })
        .await
        .map_err(|_elapsed| HandshakeError::HandshakeTimeout)??;

    sm.complete_handshake()
        .map_err(|v| HandshakeError::ProtocolViolation(v.reason))?;

    // Spec 4.7 AUTH_TIMEOUT (60s): bounds the AuthPubkey/AuthResult round
    // trip (see the client side's matching comment on the retry-loop
    // caveat).
    let auth_pubkey: AuthPubkey = tokio::time::timeout(auth_timeout, async {
        let (type_raw, payload) = reader
            .read_envelope(StreamKind::Control.max_envelope_length())
            .await?;
        check(&sm, type_raw)?;
        let auth_pubkey: AuthPubkey = messages::decode(&payload).map_err(HandshakeError::Decode)?;
        Ok::<_, HandshakeError>(auth_pubkey)
    })
    .await
    .map_err(|_elapsed| HandshakeError::AuthTimeout)??;

    let context =
        auth::channel_binding_context(&client_hello_bytes, &server_hello_bytes, &auth_challenge);
    let verification = (|| -> Result<(), auth::AuthError> {
        if auth_pubkey.public_key != trusted_public_key.as_bytes() {
            return Err(auth::AuthError::SignatureInvalid);
        }
        let exporter = auth::compute_exporter(connection, &context)?;
        auth::verify_exporter(&auth_pubkey.public_key, &exporter, &auth_pubkey.signature)
    })();

    match verification {
        Ok(()) => {
            sm.complete_authentication()
                .map_err(|v| HandshakeError::ProtocolViolation(v.reason))?;

            let mut session_id = [0u8; 16];
            rand::rng().fill_bytes(&mut session_id);
            let mut reconnect_token = [0u8; 32];
            rand::rng().fill_bytes(&mut reconnect_token);
            let granted_permissions = permission_set::bit::VIEW
                | permission_set::bit::INPUT_KEYBOARD
                | permission_set::bit::INPUT_MOUSE;

            let auth_result = AuthResult {
                status: AuthStatus::Ok,
                reason: ReasonCode::NONE,
                session_id,
                reconnect_token,
                granted_permissions,
            };
            write_envelope(
                &mut send,
                messages::type_id::AUTH_RESULT,
                &messages::encode(&auth_result),
            )
            .await?;
            let outcome = HandshakeOutcome {
                session_id,
                reconnect_token,
                granted_permissions,
            };
            Ok((outcome, sm, ControlChannel { send, reader }))
        }
        Err(auth_error) => {
            let violation = sm.record_auth_failure();
            let auth_result = AuthResult {
                status: AuthStatus::Denied,
                reason: ReasonCode::AUTH_SIGNATURE_INVALID,
                session_id: [0; 16],
                reconnect_token: [0; 32],
                granted_permissions: 0,
            };
            write_envelope(
                &mut send,
                messages::type_id::AUTH_RESULT,
                &messages::encode(&auth_result),
            )
            .await?;
            let _ = violation;
            Err(HandshakeError::Auth(auth_error))
        }
    }
}
