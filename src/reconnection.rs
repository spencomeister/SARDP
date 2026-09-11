//! Reconnection (spec 4.6): a client that already completed the M2
//! handshake once can resume a `Suspended` session on a *new* QUIC
//! connection by presenting `SessionReauthenticate` instead of
//! `ClientHello` as the first message on the new connection's `control`
//! stream, along with the `reconnect_token` issued at the end of the
//! original handshake (or a previous reconnection).
//!
//! Unlike the initial handshake, this does *not* re-verify `AuthPubkey`'s
//! signature: possession of the (short-lived, single-use, atomically
//! consumed -- see [`crate::session_store`]) `reconnect_token` is spec
//! 4.6's sole proof of continuity. This means the protocol does not, by
//! itself, verify that the device reconnecting is the same one that
//! authenticated originally -- which is exactly the concern spec 4.9's
//! still-open item ("再接続時にCapability/DisplayCapabilitiesを再申告する
//! かどうか") is about, from the display-capability angle. This
//! implementation deliberately leaves that open per the current spec
//! default (no re-declaration, keep the original session's settings,
//! confirmed sufficient for this PoC's same-device reconnect scenario);
//! revisit if/when a real "reconnect from a different device" use case
//! needs it.

use ed25519_dalek::VerifyingKey;
use rand::Rng;

use crate::connection_sm::ConnectionSm;
use crate::handshake::{ControlChannel, HandshakeError};
use crate::messages::{self, AuthResult, AuthStatus, ReauthenticateReason, SessionReauthenticate};
use crate::prologue;
use crate::reason_code::ReasonCode;
use crate::session_store::{ReconnectError, SessionStore};
use crate::stream_kind::StreamKind;
use crate::stream_reader::{EnvelopeReader, write_envelope};

/// Accepts a new incoming `control` stream and validates its
/// `StreamPrologue`, without assuming what the first Envelope on it will
/// be. Spec 4.6: a new connection's first control message is either
/// `ClientHello` (a fresh session -- see [`crate::handshake`]) or
/// `SessionReauthenticate` (resuming a `Suspended` one, this module).
pub async fn accept_control_prologue(
    connection: &quinn::Connection,
) -> Result<(quinn::SendStream, EnvelopeReader), HandshakeError> {
    let (send, recv) = connection.accept_bi().await.map_err(HandshakeError::Quic)?;
    let mut reader = EnvelopeReader::new(recv);
    let stream_prologue = reader.read_prologue().await?;
    if stream_prologue.kind != StreamKind::Control {
        // Spec 2.2.1: "未認証状態でcontrol以外のストリームが開かれたら即切断".
        return Err(HandshakeError::ProtocolViolation(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
        ));
    }
    Ok((send, reader))
}

/// The first Envelope on a newly accepted `control` stream, classified per
/// spec 4.6.
pub enum FirstControlMessage {
    /// Raw `ClientHello` bytes -- decoding is `crate::handshake`'s job
    /// (`server_handshake_from_client_hello` wants the bytes, not the
    /// decoded struct, for the channel-binding context).
    ClientHello(Vec<u8>),
    SessionReauthenticate(SessionReauthenticate),
}

/// Accepts a new `control` stream and reads its first Envelope far enough
/// to tell a fresh `ClientHello` apart from a `SessionReauthenticate`
/// (spec 4.6), without otherwise driving either the handshake or the
/// reconnection protocol -- that's the caller's job, using whichever of
/// [`crate::handshake::server_handshake_from_client_hello`] or
/// [`server_complete_reconnect`] this result calls for. Bounded by
/// `handshake_deadline` (spec 4.7's `HANDSHAKE_TIMEOUT`) -- a shared
/// deadline, not a fresh per-call duration, so this wait and whatever the
/// caller bounds by the same deadline afterwards (e.g.
/// `server_handshake_from_client_hello`'s `ServerHello` send) together
/// spend at most one `HANDSHAKE_TIMEOUT` budget rather than one each (see
/// [`establish_connection`]).
pub async fn read_first_control_message(
    connection: &quinn::Connection,
    handshake_deadline: tokio::time::Instant,
) -> Result<(quinn::SendStream, EnvelopeReader, FirstControlMessage), HandshakeError> {
    let (send, mut reader) = accept_control_prologue(connection).await?;
    let (type_raw, payload) = tokio::time::timeout_at(
        handshake_deadline,
        reader.read_envelope(StreamKind::Control.max_envelope_length()),
    )
    .await
    .map_err(|_elapsed| HandshakeError::HandshakeTimeout)??;

    let message = if type_raw == messages::type_id::CLIENT_HELLO {
        FirstControlMessage::ClientHello(payload)
    } else if type_raw == messages::type_id::SESSION_REAUTHENTICATE {
        let reauth: SessionReauthenticate =
            messages::decode(&payload).map_err(HandshakeError::Decode)?;
        FirstControlMessage::SessionReauthenticate(reauth)
    } else {
        return Err(HandshakeError::ProtocolViolation(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
        ));
    };
    Ok((send, reader, message))
}

/// Everything `sardp-server` needs to enter the Active state on a new
/// connection, unified across both ways spec 4.6 lets a connection's first
/// control message establish a session: a fresh `ClientHello` handshake,
/// or a `SessionReauthenticate` resuming a `Suspended` one.
pub struct ConnectionContext {
    pub connection_sm: ConnectionSm,
    pub control: ControlChannel,
    pub session_id: [u8; 16],
    pub user_id: String,
    pub reconnect_token: [u8; 32],
    pub granted_permissions: u32,
    /// The video Channel's next Instance should open at this generation:
    /// `0` for a fresh handshake, `ReconnectOutcome::resumed_generation`
    /// (`last_generation + 1`, DR-026) for a resumed session.
    pub starting_generation: u64,
    pub is_resumed: bool,
}

/// The result of [`establish_connection`]: either a session ready to enter
/// the Active state, or a reconnect attempt spec 4.6 rejected (the peer
/// has already been sent `AuthResult{DENIED}` by
/// [`server_complete_reconnect`] by the time this is returned -- there is
/// nothing left to do but log `reason` and end the connection).
pub enum EstablishOutcome {
    Established(ConnectionContext),
    ReconnectRejected(ReasonCode),
}

/// Accepts a new connection's `control` stream and drives it all the way
/// to the Active state's threshold (spec 4.6): reads the first Envelope to
/// tell a fresh `ClientHello` apart from a `SessionReauthenticate`, then
/// completes whichever of the handshake or the reconnection protocol
/// applies. `handshake_timeout` is a single spec 4.7 `HANDSHAKE_TIMEOUT`
/// budget computed into one deadline here and shared by both the
/// `ClientHello`-wait and (on that path) `server_handshake_from_client_hello`'s
/// `ServerHello` send, rather than each independently getting a fresh
/// `handshake_timeout` window (KNOWN_ISSUES.md #6). `auth_timeout` bounds
/// the separate `AuthPubkey`/`AuthResult` round trip as before.
pub async fn establish_connection(
    connection: &quinn::Connection,
    server_name: &str,
    trusted_pubkey: &VerifyingKey,
    sessions: &SessionStore,
    handshake_timeout: std::time::Duration,
    auth_timeout: std::time::Duration,
) -> Result<EstablishOutcome, HandshakeError> {
    let handshake_deadline = tokio::time::Instant::now() + handshake_timeout;
    let (send, reader, first_message) =
        read_first_control_message(connection, handshake_deadline).await?;
    match first_message {
        FirstControlMessage::ClientHello(client_hello_bytes) => {
            let (outcome, connection_sm, control) =
                crate::handshake::server_handshake_from_client_hello(
                    send,
                    reader,
                    client_hello_bytes,
                    connection,
                    server_name,
                    trusted_pubkey,
                    handshake_deadline,
                    auth_timeout,
                )
                .await?;
            Ok(EstablishOutcome::Established(ConnectionContext {
                connection_sm,
                control,
                session_id: outcome.session_id,
                user_id: outcome.user_id,
                reconnect_token: outcome.reconnect_token,
                granted_permissions: outcome.granted_permissions,
                starting_generation: 0,
                is_resumed: false,
            }))
        }
        FirstControlMessage::SessionReauthenticate(reauth) => {
            match server_complete_reconnect(send, reader, &reauth, sessions).await {
                Ok((outcome, connection_sm, control)) => {
                    Ok(EstablishOutcome::Established(ConnectionContext {
                        connection_sm,
                        control,
                        session_id: outcome.session_id,
                        user_id: outcome.user_id,
                        reconnect_token: outcome.reconnect_token,
                        granted_permissions: outcome.granted_permissions,
                        starting_generation: outcome.resumed_generation,
                        is_resumed: true,
                    }))
                }
                Err(e) => Ok(EstablishOutcome::ReconnectRejected(e.reason_code())),
            }
        }
    }
}

/// The outcome of a successful reconnection: same shape as
/// [`crate::handshake::HandshakeOutcome`], plus the generation the
/// resumed video Channel's next Instance should open at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectOutcome {
    pub session_id: [u8; 16],
    /// Freshly issued: spec 4.6 requires the old token be invalidated and
    /// a new one issued on every successful reconnect.
    pub reconnect_token: [u8; 32],
    pub granted_permissions: u32,
    /// `last_generation + 1` (spec 4.6, DR-026): the video Channel's next
    /// Instance must open at this generation, continuing the session's
    /// monotonic counter rather than resetting it.
    pub resumed_generation: u64,
    /// `AuthPubkey.user_id`, carried through so a session that suspends
    /// again after being resumed still has it (see
    /// `crate::session_store::SuspendedSession::user_id`).
    pub user_id: String,
}

/// Client side: opens a new `control` stream on `connection` and sends
/// `SessionReauthenticate{RECONNECT}`, then waits for the resulting
/// `AuthResult`. `connection_sm` must already be `Suspended` (detecting
/// that the original connection was lost is the caller's job); on success
/// it's resumed to `Active` (spec 4.6) as a side effect of this call.
pub async fn client_reconnect(
    connection: &quinn::Connection,
    connection_sm: &mut ConnectionSm,
    prior_session_id: [u8; 16],
    reconnect_token: [u8; 32],
    user_id: &str,
) -> Result<(ReconnectOutcome, ControlChannel), HandshakeError> {
    let (mut send, recv) = connection.open_bi().await.map_err(HandshakeError::Quic)?;
    let mut reader = EnvelopeReader::new(recv);

    let mut prologue_bytes = Vec::new();
    prologue::encode(StreamKind::Control, 1, 0, &mut prologue_bytes);
    send.write_all(&prologue_bytes).await?;

    let reauth = SessionReauthenticate {
        reason: ReauthenticateReason::Reconnect,
        prior_session_id,
        reconnect_token,
    };
    write_envelope(
        &mut send,
        messages::type_id::SESSION_REAUTHENTICATE,
        &messages::encode(&reauth),
    )
    .await?;

    let (type_raw, payload) = reader
        .read_envelope(StreamKind::Control.max_envelope_length())
        .await?;
    if type_raw != messages::type_id::AUTH_RESULT {
        return Err(HandshakeError::ProtocolViolation(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
        ));
    }
    let auth_result: AuthResult = messages::decode(&payload).map_err(HandshakeError::Decode)?;

    match auth_result.status {
        AuthStatus::Ok => {
            connection_sm
                .resume()
                .map_err(|v| HandshakeError::ProtocolViolation(v.reason))?;
            let outcome = ReconnectOutcome {
                session_id: prior_session_id,
                reconnect_token: auth_result.reconnect_token,
                granted_permissions: auth_result.granted_permissions,
                // The client doesn't independently track the Channel's
                // generation here; the server is authoritative and the
                // resumed video stream's own `VideoStreamGeneration`
                // message (spec 2.10) is what actually tells the client
                // which generation it's on.
                resumed_generation: 0,
                user_id: user_id.to_string(),
            };
            Ok((outcome, ControlChannel { send, reader }))
        }
        AuthStatus::MfaRequired | AuthStatus::Denied => Err(HandshakeError::AuthDenied),
    }
}

/// Everything that can go wrong completing a reconnection server-side.
#[derive(Debug)]
pub enum ReconnectServerError {
    Store(ReconnectError),
    Write(quinn::WriteError),
    Violation(ReasonCode),
}

impl From<quinn::WriteError> for ReconnectServerError {
    fn from(e: quinn::WriteError) -> Self {
        Self::Write(e)
    }
}

impl ReconnectServerError {
    /// The `ReasonCode` `AuthResult.reason` was (or would be) set to for
    /// this failure (spec 4.6's error table: both
    /// [`ReconnectError`] variants map to the same wire code).
    pub fn reason_code(&self) -> ReasonCode {
        match self {
            Self::Store(_) => ReasonCode::AUTH_RECONNECT_TOKEN_INVALID,
            Self::Violation(reason) => *reason,
            Self::Write(_) => ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
        }
    }
}

/// Server side: given the `SessionReauthenticate` just read off `reader`
/// (by the caller, since deciding *that* it's a reconnect rather than a
/// fresh `ClientHello` happens one layer up -- see the module docs),
/// validates and atomically consumes the `reconnect_token` against
/// `store`, and sends back the resulting `AuthResult` (`OK` with a fresh
/// token on success, per spec 4.6's `ReconnectAccepted`; `DENIED` with
/// [`ReconnectServerError::reason_code`] otherwise, `ReconnectRejected`).
pub async fn server_complete_reconnect(
    mut send: quinn::SendStream,
    reader: EnvelopeReader,
    reauth: &SessionReauthenticate,
    store: &SessionStore,
) -> Result<(ReconnectOutcome, ConnectionSm, ControlChannel), ReconnectServerError> {
    match store.try_reconnect(reauth.prior_session_id, reauth.reconnect_token) {
        Ok(mut suspended) => {
            suspended
                .connection_sm
                .resume()
                .map_err(|v| ReconnectServerError::Violation(v.reason))?;

            let mut new_reconnect_token = [0u8; 32];
            rand::rng().fill_bytes(&mut new_reconnect_token);

            let auth_result = AuthResult {
                status: AuthStatus::Ok,
                reason: ReasonCode::NONE,
                session_id: reauth.prior_session_id,
                reconnect_token: new_reconnect_token,
                granted_permissions: suspended.granted_permissions,
            };
            write_envelope(
                &mut send,
                messages::type_id::AUTH_RESULT,
                &messages::encode(&auth_result),
            )
            .await?;

            let outcome = ReconnectOutcome {
                session_id: reauth.prior_session_id,
                reconnect_token: new_reconnect_token,
                granted_permissions: suspended.granted_permissions,
                resumed_generation: suspended.last_generation + 1,
                user_id: suspended.user_id.clone(),
            };
            Ok((
                outcome,
                suspended.connection_sm,
                ControlChannel { send, reader },
            ))
        }
        Err(store_error) => {
            let auth_result = AuthResult {
                status: AuthStatus::Denied,
                reason: ReconnectServerError::Store(store_error).reason_code(),
                session_id: [0; 16],
                reconnect_token: [0; 32],
                granted_permissions: 0,
            };
            // Best-effort: the caller learns about the rejection via the
            // returned Err regardless of whether this send itself succeeds.
            let _ = write_envelope(
                &mut send,
                messages::type_id::AUTH_RESULT,
                &messages::encode(&auth_result),
            )
            .await;
            Err(ReconnectServerError::Store(store_error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_server_error_reason_codes_match_spec_4_6() {
        assert_eq!(
            ReconnectServerError::Store(ReconnectError::NoSuchSession).reason_code(),
            ReasonCode::AUTH_RECONNECT_TOKEN_INVALID
        );
        assert_eq!(
            ReconnectServerError::Store(ReconnectError::TokenMismatch).reason_code(),
            ReasonCode::AUTH_RECONNECT_TOKEN_INVALID
        );
    }
}
