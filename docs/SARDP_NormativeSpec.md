# SARDP 規範仕様 v0.3

本書は v0.2 を置き換える、実装者向けの現行規範仕様である。設計の経緯・棄却案の理由は `sardp-schema-v0.1.md`(設計史ジャーナル)を参照。各所の DR-xxx は Appendix A の Decision Record 索引に対応する。

キーワード MUST / MUST NOT / SHOULD / MAY は RFC 2119 の意味で用いる。

**v0.2からの主な変更点**: 映像ストリームのgeneration管理を追加、ストリームの初期化方向を明示、TransportFeedbackを拡張しバックプレッシャ判定をQUIC実装非依存の信号に一本化、Bフレームを禁止しdecode_order/display_orderを不要化、StreamPrologueにmagic numberを追加しEnvelope外の固定構造として明確化、H.264のSPS/PPS運用規則を追加、認証をPUBLIC_KEY/PASSKEYのみ必須としPASSWORD/TOTPは定義を先送り、AuthPolicyに優先順位を追加、クリップボードにrequest_idを追加、ファイル転送の完全性検証を強化、音声のクロックドリフト補正を追加、リレーの暗号境界を明文化、Envelopeとメッセージ本体のシリアライゼーション方針を分離。

**本改訂(Part 4)での変更点**: Part 4を8種の状態機械(Connection / Stream Lifecycle / VideoStream / Input / Permission / Reconnection)+タイムアウトモデル+エラーハンドリングマトリクスへ再構成。VideoStream SMをChannel(モニター単位)とInstance(QUICストリーム単位)の2層に分離し、2.10節と2.14節の異なる劣化基準が旧Part 4で暗黙に混同されていた点を明確化。CLIENT_SIDE IMEモード中の生KeyEvent抑制ルール(二重入力防止)を新たに確定。

**本改訂(数値決定)での変更点**: 4.9節にあった10件の未決定タイムアウト値のうち8件を、根拠とサブエージェントによる妥当性検証を経て確定した(4.7節)。この過程で2.10節の映像バックプレッシャ閾値に欠陥(絶対値では高遅延経路の伝搬遅延を輻輳と誤検知する)が見つかり、ベースライン相対のdeltaへ再定義した(DR-029)。KeepAliveメッセージを新たに定義し(2.9節)、VideoChannelのリカバリ失敗時の挙動を「無期限バックオフ再試行」に変更した(DR-030、恒久的な諦め状態を撤廃)。残る未決定事項は数値ではない挙動2件のみ(4.9節)。DR-028〜DR-031を追加。

---

# Part 1. Protocol Overview

目的・主要判断はv0.2と同一(DR-001〜DR-013)。加えて本版では以下を規範として確定する。

7. 映像ストリームの回復(RESET_STREAM+再オープン)には明示的な世代番号を付け、クライアントが古い世代のデータを確実に破棄できるようにする(DR-017)。
8. すべてのストリームについて、初期化可能な側(client-initiated / server-initiated)と方向性(unidirectional / bidirectional)を固定する(DR-018)。
9. Bフレームは禁止する。送信順・デコード順・表示順を一致させ、複雑性を削る(DR-019)。

---

# Part 2. Normative Wire Specification

## 2.1 共通規約

```text
バイト順        : リトルエンディアン
可変長整数      : QUIC varint (RFC 9000 §16)
文字列          : UTF-8、長さ前置。最大256バイト、NUL禁止、C0/C1制御文字禁止。
時刻            : マイクロ秒、送信者の単調時計基準。壁時計はワイヤに載せない。
ID空間          : frame_id, event_id, request_id, sequence, generation は
                  スコープごとに独立、0起点単調増加
```

### シリアライゼーション方針(DR-021)

- **Envelope(下記2.1.1)**: 意図的に手書きの最小固定パーサーとする。小さく監査しやすいことが目的であり、「手書きパーサー禁止」の例外として扱う。ただし Part 8 の要求どおり、このパーサーは MUST でファジング対象に含める。
- **メッセージ本体**(control/input/feedback/clipboardの構造化メッセージ): スキーマ生成コード(FlatBuffers、Protobuf、CBOR等、実装が選択)でエンコードする。1実装内では方式を統一し、選択した方式を実装ドキュメントに明記する。
- **映像・音声・ファイルのペイロード**: 生バイト列のまま扱う。ゼロコピーを保つため、スキーマでラップしない。

### バージョニング(DR-016)

- QUICバインディング: ALPN `sardp/1`
- TCPフォールバックバインディング: ALPN `sardp-tcp/1`(2.16節)
- Capabilityで表現可能な変更はALPNを変えずに行える。Envelope形式・認証署名対象・StreamPrologue構造の変更など後方互換に解釈できない変更はALPNを上げる(`sardp/2`)ことをMUSTとする。

### 2.1.1 Envelope(全ストリーム共通、StreamPrologueの後に繰り返される)

```text
Envelope {
    length   : varint
    type     : u16
    payload  : bytes
}
```

**`length` は `payload` のみのバイト数を表す(`type` フィールドは含まない、DR-032)**。`type` は常に2バイト固定長で存在するため、`length` に含めると `length < 2` という到達不能なエラー状態を導入するだけで得るものがない。ヘッダ全体(varintのバイト長 + 2)を除いた残りのバイト数が `length` である。この規約はHTTP/2フレームヘッダおよびTLSレコード層の `length` フィールド(いずれも自身のヘッダは含めない)の慣行に倣う。実装は `length` を受信した時点で、後続バイトの到着を待たずに当該ストリームの上限(下表)と比較してよい(SHOULD)。

`type` のビット配置(DR-014):

```text
bit15 (0x8000)  : ignorable flag (1 = 未知でも無視可)
bit14:0         : type id
    0x0000–0x3FFF : core        (仕様定義、当該ストリームでは実装MUST)
    0x4000–0x7FFF : experimental (ベンダー拡張、実装MAY)
```

未知の `type` を受信した場合:

- `ignorable flag = 1`: `length` に従いpayloadをそのままスキップし、状態変化は一切行わない。以降のバイトの解析は継続する。
- `ignorable flag = 0`: MUSTでストリームを異常終了させる(controlなら接続ごと切断)。

ストリーム種別ごとの `length` 上限:

| ストリーム | length上限 |
|---|---|
| control | 64 KiB |
| input | 1 KiB |
| video | 8 MiB |
| feedback | 4 KiB |
| audio_playback / audio_capture | 64 KiB |
| clipboard | 16 MiB |
| file | 1 MiB(チャンク単位) |

## 2.2 StreamPrologue(DR-006, DR-015)

新規ストリームを開く側は、**Envelopeでラップしない固定構造**を、そのストリームの最初のバイトとして送る(DR-015)。

```text
StreamPrologue {
    magic      : bytes(4)   // "SARD" = 0x53 0x41 0x52 0x44。フレーミング誤りの早期検出用
    kind       : u8
    version    : u8         // 当該kindのメッセージ体系バージョン。現在は常に1
    context_id : varint     // kindごとの意味は下表
}
```

`magic` が一致しない、または `kind` が未知の値であれば、実装はMUSTでそのストリームを異常終了させる。

### 2.2.1 ストリーム種別・初期化方向・context_id(DR-018)

| kind | 値 | 初期化側 | 方向性 | context_id |
|---|---|---|---|---|
| control | 0x01 | client | 双方向 | 0(未使用) |
| input | 0x02 | client | 単方向(client→server) | 0(未使用) |
| feedback | 0x04 | client | 単方向(client→server) | 0(未使用) |
| video | 0x03 | server | 単方向(server→client) | monitor_id |
| audio_playback | 0x07 | server | 単方向(server→client) | 0(将来デバイス複数化時に使用) |
| audio_capture | 0x08 | client | 単方向(client→server) | 0(同上) |
| clipboard | 0x05 | **クリップボード内容が変化した側**(client/serverいずれもあり得る) | 双方向 | request_id |
| file | 0x06 | `direction=UPLOAD`ならclient、`DOWNLOAD`ならserver | 単方向(データはdirectionが示す向きのみ) | file_handle |

不変条件(MUST):

- 未認証状態で `control` 以外のストリームが開かれたら即切断する。
- 表の初期化側と異なる側がそのkindのストリームを開こうとした場合、プロトコル違反として接続を切断する。
- 同一 `context_id` を持つ `video` ストリームが同時に複数Activeな状態を検出した場合、クライアントはより新しい `generation`(2.10節)を持つ方を正としてもう一方を無視する。実装はこの状態が長時間続かないよう努める(SHOULD)。

## 2.3 ハンドシェイクと認証(DR-005, DR-020)

```text
ClientHello {
    client_name, client_version : string
    capabilities  : [CapabilityId]
    auth_methods  : [AuthMethod]
}

ServerHello {
    server_name, server_version : string
    capabilities   : [CapabilityId]
    auth_policy    : AuthPolicy
    auth_challenge : bytes(32)     // one-time。再送・再利用禁止
}

AuthMethod : enum { PUBLIC_KEY, PASSKEY, PASSWORD, TOTP }

AuthCombination {
    methods  : [AuthMethod]
    priority : u8      // 小さいほどサーバー推奨度が高い
}

AuthPolicy {
    accepted_combinations : [AuthCombination]
    // 例: [ {[PASSKEY], 0}, {[PUBLIC_KEY], 1}, {[PASSWORD, TOTP], 2} ]
}
```

**v0.3で必須実装とする認証方式は `PUBLIC_KEY` と `PASSKEY` のみ**(DR-020)。`PASSWORD` / `TOTP` はAuthMethod enum値・Capability広告としては予約するが、具体的なメッセージ形式(チャレンジ方式、パスワード検証、試行回数制限等)の定義は本版では行わない。実装がこれらを提供する場合は、実装独自の拡張(0x4000–0x7FFF namespace)として定義し、将来版での正式化を待つ。

```text
AuthPubkey {
    user_id, device_id : string
    public_key : bytes
    signature   : bytes
}
```

署名対象(チャネルバインディング、RFC 9266準拠):

```text
context  = "SARDP-auth-v1" || hash(ClientHello) || hash(ServerHello) || auth_challenge
exporter = TLS-Exporter(label="EXPORTER-SARDP-auth-v1", context=context, length=32)
signature の対象 = exporter
```

`AuthPasskeyAssertion` も同様の枠組みで、WebAuthn assertion結果を `signature` 相当として扱う(詳細はWebAuthn仕様に委譲)。

### 認証の再試行とチャレンジ管理

```text
AuthChallengeRenew { auth_challenge : bytes(32) }   // control, server→client
```

- `auth_challenge` はMUSTで一度のみ使用可能。
- 認証失敗後にクライアントが再試行する場合、サーバーは新しい `auth_challenge` を含む `AuthChallengeRenew` を送るか、接続を切断する。
- コネクションあたりの認証試行回数は既定3回まで(SHOULD、設定可能)。上限に達したらMUSTで接続を切断する。
- アカウント単位のロックアウト・遅延はポリシー層の実装事項とし、ワイヤには現れない。

```text
AuthResult {
    status              : enum { OK, MFA_REQUIRED, DENIED }
    reason              : ReasonCode
    session_id          : bytes(16)
    reconnect_token      : bytes(32)
    granted_permissions  : PermissionSet
}
```

`MFA_REQUIRED` を受け取ったクライアントは、`accepted_combinations` のうち追加要素を含む組を選び直し、不足分の認証メッセージを同一 control ストリーム上で追加送信する。

0-RTT early dataでは `control` ストリームの認証メッセージ以外を一切受け付けない(初期版は0-RTT自体を無効化)。

### 再接続(レビュー指摘#5、変更なし)

```text
SessionReauthenticate {
    reason           : enum { RECONNECT, PERMISSION_REFRESH }
    prior_session_id : bytes(16)
    reconnect_token  : bytes(32)
}
```

`reconnect_token` の消費はMUSTでアトミックに行う(クラスタ構成での同時消費レースを防ぐ)。成功時は新しい `AuthResult` を返し、旧トークンを無効化する。

## 2.4 セッション確立・表示構成

```text
DisplayConfig {
    config_id : u64
    monitors  : [{
        monitor_id, x, y, width, height : (前版と同一)
        scale_percent, rotation, primary
    }]
}

ActiveMonitor { monitor_id : u8 }   // control、client→server
```

## 2.5 権限

```text
PermissionSet : bitflags(u32) {
    VIEW, INPUT_KEYBOARD, INPUT_MOUSE,
    CLIP_READ, CLIP_WRITE,
    FILE_UP, FILE_DOWN,
    AUDIO_PLAYBACK, AUDIO_CAPTURE,
    ADMIN
}

PermissionUpdate {                 // control、server→client
    granted_permissions : PermissionSet
    immediate_revoke     : PermissionSet
}
```

**ビット割り当て(DR-033)**:

| bit | フラグ |
|---|---|
| 0 | VIEW |
| 1 | INPUT_KEYBOARD |
| 2 | INPUT_MOUSE |
| 3 | CLIP_READ |
| 4 | CLIP_WRITE |
| 5 | FILE_UP |
| 6 | FILE_DOWN |
| 7 | AUDIO_PLAYBACK |
| 8 | AUDIO_CAPTURE |
| 9 | ADMIN |

bit 10〜31は将来の拡張用に予約する。宣言順=ビット番号順という規約は、今後フラグを追加する際も末尾に足す限り既存ビットと衝突しない。

`granted_permissions` は常に現在の実効許可状態を表す。`immediate_revoke` に含まれる権限は進行中の操作があっても即座に無効化し、含まれない権限(`FILE_*`)は進行中の転送を完了まで許容し以降の新規リクエストのみ拒否する。`CLIP_*` は次回の `ClipboardRequest` から新しい状態を適用する。

*(この方式が正式版である。v0.1 設計史内の `effective_immediately: bool` 案は棄却済み。詳細はv0.1のErrata参照)*

## 2.6 ファイル転送(レビュー指摘#7, #12)

```text
FileTransferRequest {              // control、常にクライアントが送信する要求
    request_id    : varint
    direction     : enum { UPLOAD, DOWNLOAD }
    virtual_path  : string
    declared_size : u64            // 未検証のヒント値
}

FileTransferAccept {               // control、server→client
    request_id    : varint
    file_handle   : bytes(16)      // 不透明ハンドル。セッション・ユーザー・方向・有効期限に束縛
    resolved_size : u64
    expiry_ts     : u64            // このfile_handleが有効な期限(単調時計基準)
}

FileTransferReject { request_id, reason : ReasonCode }
```

サーバー側処理パイプライン(MUST): `Request → ポリシーチェック → virtual_pathの正規化 → サンドボックスチェック(symlink/junction/UNC/マウント越境を含む) → file_handle発行 → 転送 → 整合性検証`。

`file_handle` 発行後、`direction` が示す側が `file` ストリームを開く(`StreamPrologue.context_id = file_handle`)。

```text
FileChunk {
    offset : u64
    length : u32     // dataの長さ。Envelope側の長さと独立に検証する
    data   : bytes
}

FileTransferComplete { checksum : bytes(32) }   // SHA-256、全体に対して計算

FileTransferError {                              // control、いずれの側からも送信可
    request_id または file_handle
    reason : ReasonCode
}
```

受信側はMUSTで以下を拒否する: オフセットの重複、`resolved_size` を超える範囲、穴のある状態での `FileTransferComplete` 受理、`checksum` 不一致。いずれの場合も `FileTransferError` を送って転送を中断する。

再開は `file_handle` 単独では許可しない。再接続後に転送を再開する場合、クライアントはMUSTで元の `session_id`/`user_id` に紐づくセッションとして再認証し(2.3節)、`expiry_ts` 内であることをサーバーが確認した上で、既知の `offset` から `FileChunk` を再送する。

## 2.7 クリップボード(レビュー指摘#8, #11)

```text
ClipboardFormats {                 // clipboardストリーム、announcer側が送信
    request_id : varint            // この一連のやり取りを識別
    formats    : [{ namespace : FormatNamespace, format_id : string }]
}

FormatNamespace : enum { MIME, WIN32, MACOS_UTI }

ClipboardRequest {                 // clipboardストリーム、要求側が送信
    request_id : varint            // 対応するClipboardFormatsのrequest_idと一致させる
    namespace  : FormatNamespace
    format_id  : string
}

ClipboardData {
    request_id : varint
    namespace, format_id : (上記と同型)
    data : bytes
}

ClipboardError { request_id, reason : ReasonCode }
```

`request_id` は当該 `clipboard` ストリーム(StreamPrologue.context_id と同値)のスコープで一意。形式ごとのデータサイズはストリームのlength上限(16MiB)に加え、ポリシーで個別の上限を設定してよい(MAY)。要求後、既定5秒(SHOULD、設定可能)以内に `ClipboardData` も `ClipboardError` も届かない場合、要求側はタイムアウトとして扱ってよい。

## 2.8 理由コード

```text
ReasonCode {
    domain : u8    // 0=NONE(エラーなし、予約), 1=AUTH, 2=POLICY, 3=TRANSPORT, 4=PROTOCOL, 5=OS
    code   : u16
}
```

**`domain = 0, code = 0` はエラーなしを表す予約値(DR-034)**である。`AuthResult`等、成功時にも `ReasonCode` フィールドを送る必要がある(構造体の必須フィールドである)メッセージでは、`status = OK` の場合MUSTで `ReasonCode{domain: 0, code: 0}` を設定する。個々のドメイン内の具体的な `code` の割り当ては4.8節のReasonCode一覧を参照。

## 2.9 時刻同期(レビュー指摘#3(前版), #8)

```text
TimeSyncRequest  { t1 : u64 }              // control、どちらからでも開始可
TimeSyncResponse { t1 : u64, t2 : u64, t3 : u64 }
```

```text
offset = ((t2 - t1) + (t3 - t4)) / 2
rtt    = (t4 - t1) - (t3 - t2)
```

(`t4` はワイヤに乗らない。要求側がローカルに記録する。)

規範:

- `t1`〜`t4` はすべて送信者ローカルの単調時計値であり、単位はMUSTでマイクロ秒とする。
- セッション中はMUSTで同一の時計源を使用する。
- 再接続後は前セッションの時計基準をMUST NOTで引き継がない。新セッション確立後、改めて `TimeSyncRequest`/`Response` を行う。
- 異なるセッション間の時刻はMUST NOTで比較しない。
- `TimeSyncResponse` はMUSTで `control` ストリームに固定する(v0.1設計史にある「feedbackでも可」という記述は本版で正式に削除する)。
- 実装は `TimeSyncResponse` を他の control メッセージより優先して送出すべきである(SHOULD)。

### KeepAlive

```text
KeepAlive {}   // control、双方向、既定 KEEPALIVE_INTERVAL = 15秒 ごとに送出(SHOULD)
```

15秒間隔は、QUIC/UDPのNATバインディング維持における一般的な慣行(RFC 7675のconsent freshness相当の考え方)に基づく既定値であり、家庭用・モバイルキャリアのNATに多いUDPマッピングのタイムアウト(短いもので30秒程度)に対して安全マージンを確保する。実装は経路がモバイル/セルラーであると判定できる場合、より短い間隔に調整してよい(MAY)。

## 2.10 映像(DR-001, DR-006, DR-007, DR-017, DR-019)

映像はモニターごとに1本の長寿命な信頼性付きストリームを既定とする。ストリーム開始直後、最初のメッセージとして当該モニター向けの `VideoStreamGeneration` と `EncoderConfig` を順に送る。

```text
VideoStreamGeneration {
    generation : u64      // このストリームインスタンスの世代番号。0起点、再オープンのたびに+1
    config_id  : u64      // DisplayConfig.config_id への参照
}

EncoderConfig {
    codec         : enum { H264 }
    profile       : u16
    chroma_format : enum { C420, C422, C444 }
    bit_depth     : u8
    width, height : u32
    max_fps       : u16
    tier          : u8
    b_frames      : u8 = 0            // v0.3では常に0固定。将来拡張のため予約
    server_cursor_excludable : bool
}

DisplayCapabilities {               // control、client→server。デコード能力のみ申告(DR-011)
    codecs : [{
        codec, profile, bit_depth, chroma_format
        max_width, max_height, max_fps
        hardware_acceleration : bool
    }]
}

**`VideoFrame` はワイヤ上で2つの連続したEnvelopeに分割する(DR-035)**。DR-021の「メッセージ本体はスキーマ生成コード」「映像・音声・ファイルのペイロードは生バイト列のままゼロコピーで扱う」という2分類は、単一のCBOR構造体にヘッダとペイロードを混在させると両立しない(ペイロードが構造体の一フィールドである以上、デコード時にスキーマ層が確保する新規バッファへコピーされてしまう)。ヘッダとペイロードをそれぞれ独立したEnvelopeとして送ることで、ペイロード側はEnvelope層(2.1.1節)が本来持つ生バイト列の扱いをそのまま利用できる。

```text
VideoFrameHeader {                 // CBOR。type_id: VIDEO_FRAME_HEADER
    generation     : varint         // このフレームが属する世代
    frame_id       : varint         // 世代内で0起点、送信順=デコード順=表示順(Bフレーム禁止のため)
    config_id      : u64
    flags          : u8             // bit0: IDR
    capture_ts     : u64
    encode_done_ts : u64
    width, height  : u32            // 解像度変更時のみ必須
    payload_len    : varint         // 直後に続くVIDEO_FRAME_PAYLOADのEnvelope.lengthと
                                     // 一致することをMUSTで検証する(PROTOCOL.8参照)
}

VideoFramePayload {                 // type_id: VIDEO_FRAME_PAYLOAD
                                     // CBORでラップしない。Envelope.payloadがそのままH.264 Annex-Bバイト列
}
```

`VideoFrameHeader` の直後に、同一ストリーム上で `VideoFramePayload` がMUSTで続く(間に他のEnvelopeを挟まない)。受信側はヘッダを読み終えた時点で `payload_len` バイトを期待し、後続Envelopeの `length` と付き合わせて `PROTOCOL.8 FRAME_LENGTH_MISMATCH` を検証する。

```text
VideoFrame {
    generation     : varint         // このフレームが属する世代
    frame_id       : varint         // 世代内で0起点、送信順=デコード順=表示順(Bフレーム禁止のため)
    config_id      : u64
    flags          : u8             // bit0: IDR
    capture_ts     : u64
    encode_done_ts : u64
    width, height  : u32            // 解像度変更時のみ必須
    payload_len    : varint         // payloadの長さ。Envelope.lengthとの整合をMUSTで検証する
    payload        : bytes          // H.264 Annex-B
}
```

上記 `VideoFrame` はワイヤ形式ではなく、実装が2つのEnvelope(`VideoFrameHeader` + `VideoFramePayload`)を受信して組み立てる論理ビューとして参照してよい。以降の本書の記述(4.3.2節等)で単に「`VideoFrame`」と言及する箇所は、この論理ビューを指す。

`monitor_id` はいずれのメッセージにも含めない。ストリーム自体がmonitor_idに束縛されている(2.2.1節)ため冗長である。

### フレーム境界とH.264運用規則(レビュー指摘#6)

- 1つの `VideoFrame` はMUSTで1表示フレームに対応する。
- IDRフレームの `payload` はMUSTでSPS/PPS NALユニットを含み、単体で自己完結的にデコード可能でなければならない。`EncoderConfig` にparameter setsを含めない(IDRごとの帯域内送出のみで十分とする)。
- 解像度変更はMUSTで新しいSPS/PPSを含む自己完結IDRとして送る。同時に `VideoFrame.width/height` を設定する。
- **Bフレームは禁止する**(`EncoderConfig.b_frames = 0` 固定)。これにより `frame_id` の送信順・デコード順・表示順が一致し、`decode_order`/`display_order` のような別フィールドは不要になる(DR-019)。

### バックプレッシャとgeneration管理(レビュー指摘#1, #3, 最重要)

送信元フレームドロップ(キャプチャ済み・未エンコードのフレームを捨てる)は将来のキュー増加を防ぐのみで、**既にストリームへ書き込み済みのバイトの遅延は救えない**。QUICストリームは順序付きバイトストリームであり、先頭が再送待ちになると後続の到着済みデータもアプリケーションには渡らないためである。

バックプレッシャ判定は、QUIC実装ごとの内部統計(`bytes_in_flight` 等)に依存させず、**既存のフィードバックループのみで完結させる**(前版のレビュー指摘#3への対応。実装依存/非依存の二層に分けるのではなく、そもそも実装依存な情報を判定の必須経路から外す):

- **主信号(MUST、実装非依存)**: `TransportFeedback.client_queue_delay_us`(サーバーの `capture_ts` から、クライアントの表示時刻までの実測遅延。TimeSyncのoffsetを用いてクライアントが算出する)。
- **補助信号(MAY)**: QUICライブラリがローカルに `bytes_in_flight` / 送信バッファ滞留量を提供する場合、サーバーはフィードバックの往復を待たずにこれらでも同様の閾値判定を行い、より早く反応してよい。

**閾値はベースライン相対のdeltaで定義する(DR-029)**。`client_queue_delay_us` は伝搬遅延(経路の物理的距離による、輻輳とは無関係な遅延)と滞留遅延(輻輳による遅延)を区別しない値である。衛星回線や大陸間経路などRTTが数百msに達する環境では、絶対値による閾値判定は輻輳が皆無でも常時発火し、正常な状態を誤って劣化と判定し続ける。これを避けるため、サーバーは当該Instanceについて `client_queue_delay_us` の直近 `VIDEO_QUEUE_BASELINE_WINDOW`(既定10秒)における観測最小値を `baseline` として継続的に追跡し、**`baseline` からの超過分(delta)** を判定に用いる。`baseline` はgeneration境界(ストリーム再開)をまたいで同一Channel内では維持し、再接続(Connection SMのSuspended→Active)時のみリセットする(経路自体が変わった可能性があるため)。

- `client_queue_delay_us - baseline > MAX_VIDEO_QUEUE_DURATION_DELTA`(既定300ms)を連続3回のfeedback intervalで超えた場合、当該映像ストリームを Recovering 状態に遷移させる。
- `client_queue_delay_us - baseline > VIDEO_RATE_REDUCE_THRESHOLD_DELTA`(既定100ms)を超えた場合、Congested状態としてビットレート・解像度・FPSを段階的に低下させる。`baseline`から`VIDEO_RATE_REDUCE_THRESHOLD_DELTA`未満に連続500ms(既定、ヒステリシス)留まればStreamingへ復帰する。

Recovering状態への遷移時、サーバーは以下を行う(MUST):

1. 現在の映像ストリームを `RESET_STREAM` で放棄する。
2. 同じ `context_id`(monitor_id)で新しいQUICストリームを開く。
3. `generation` を1つ増やした `VideoStreamGeneration` を送る。
4. 新しい `EncoderConfig` と、自己完結IDRの `VideoFrame`(`generation` は新しい値、`frame_id = 0`)を送る。

クライアント側の規範(MUST):

- 新しい `generation` を含む `VideoStreamGeneration` を受信したら、それ未満のgenerationに属する未表示フレームをすべて破棄する。
- `EncoderConfig` と当該generationの最初のIDRを受信するまで、表示をMUST NOTで更新しない(直前の表示内容を保持する)。
- ストリームのリセット通知(QUICのRESET_STREAM/STOP_SENDING相当)を受けた際、そのストリームに関して部分的にバッファ済みのフレームデータをMUSTで破棄する。

*(v0.1設計史の「RESET_STREAMで個別フレームを破棄する案は棄却」という記述と、本節の「ストリーム全体をRESETして再開する」は粒度が異なる別の機構である。前者は通常運用中に個々のフレームを間引く案で参照チェーンを壊すため棄却されたもの、後者はバックログが臨界値を超えた例外的な場合にのみストリーム全体を作り直す回復機構であり、両者は矛盾しない。)*

```text
KeyframeRequest { reason : enum { DECODE_ERROR, RECONNECT, MANUAL } }  // feedback、client→server
```

サーバー起点のバックログ由来リセットは上記の自動手順で処理し、専用メッセージは持たない。

## 2.11 カーソル(DR-004)

```text
CursorShape {
    cursor_id, width, height, hotspot_x, hotspot_y : (前版と同一)
    format            : enum { RGBA8888 }
    coordinate_space  : enum { PHYSICAL_PIXEL, LOGICAL_PIXEL }
    pixels            : bytes
}

CursorUpdate { cursor_id : u16, x, y : i32, visible : bool }
```

`server_cursor_excludable = true` の環境ではクライアント側描画。`false` の環境ではサーバー側焼き込みへフォールバックする(二重表示は採用しない)。

## 2.12 入力

```text
InputHeader { event_id : varint, client_ts : u64 }

KeyEvent {
    header, down : bool
    scancode    : u32      // USB HID Usage ID。物理キーの同一性を決定する
    logical_key : u32      // レイアウト解釈済み論理キー(0=不明)
    modifiers   : u16
}

TextInput      { header, text : string }
ImeComposition { header, text : string, caret : u16 }
MouseMove   { header, x, y : i32 }
MouseButton { header, button : u8, down : bool, x, y : i32 }
Wheel       { header, dx, dy : i16, is_precise : bool }
```

物理キーの同一性は `scancode` によって決定する。文字生成は `TextInput` に基づかなければならず(MUST)、`logical_key` から文字を合成してはならない(MUST NOT)。

```text
ImeModeChange { mode : enum { CLIENT_SIDE, REMOTE_SIDE }, effective_after_event_id : varint }
```

## 2.13 音声(レビュー指摘#7(前版), #14)

```text
AudioConfig {
    codec              : enum { OPUS }
    sample_rate        : u32
    channels           : u8
    frame_duration_ms  : u16
}

AudioFrame {
    sequence    : varint
    capture_ts  : u64      // 映像と同じ単調時計基準
    duration_us : u32
    payload_len : varint
    payload     : bytes    // Opus
}
```

ストリーム種別は `audio_playback`(server→client、0x07)と `audio_capture`(client→server、0x08)の2つで固定する(v0.1設計史にあった「専用audioストリーム(0x07)」という単一表記は本版のこの定義に統一される)。

### クロックドリフト補正

映像と音声のキャプチャ時計は同一ホストでもドリフトし得る(OS/デバイス固有のオーディオクロックのため)。

```text
AudioSyncFeedback {                // feedback、client→server、既定2秒周期(SHOULD)
    audio_played_ts    : u64       // クライアント単調時計上での、対応する音声再生時刻
    video_displayed_ts : u64       // 同時刻に表示されていた映像フレームの表示時刻
    drift_ppm           : i32
}
```

クライアントは再生レートの微調整、ジッターバッファ長の調整、音声フレームの挿入・破棄によって同期を維持する(実装詳細はSHOULD/MAYレベルの指針とし、アルゴリズムは規定しない)。

音声はv0.3でも映像と同様に信頼性付きストリームを既定とする。ただしOpusは前方参照が短く損失耐性が高いため、映像のような世代管理は不要である。クライアント側のジッターバッファ遅延が既定200ms(SHOULD)を超えた場合、クライアントは古い音声フレームを破棄して追いつく(skip-ahead)ことができる(MAY)。QUIC Datagramによる音声配信は将来課題とする(Appendix B)。

## 2.14 フィードバック(レビュー指摘#2)

```text
TransportFeedback {                // feedback、client→server、100ms周期+last_displayed_frame_id変化時
    last_received_frame_id  : varint
    last_decoded_frame_id   : varint
    last_displayed_frame_id : varint
    frames_received          : u32
    frames_dropped           : u32
    receive_bitrate_bps      : u64
    decode_delay_us          : u32
    display_delay_us         : u32
    target_latency_us        : u32   // クライアントが望む目標遅延(UI設定等に基づく)
    client_queue_delay_us    : u32   // capture_tsから表示までの実測遅延。2.10節の主信号
}
```

`last_received_frame_id` により、受信はできているがデコード・表示が遅れているケースと、そもそも受信自体が遅れているケースを区別できる。

### Datagram縮退モード(第2段階、規定のみ)

```text
degraded when:
    queue_delay > 150ms が3回連続のfeedback interval で観測される
    OR
    decoded_frame_age > 200ms
```

QUIC DATAGRAMもQUICの輻輳制御下にあり、輻輳制御そのものを回避する手段ではない。縮退モードの本質は「再送による遅延を捨てる」ことである。

## 2.15 監査ログ

単調シーケンスとハッシュチェーンに基づく。壁時計は補助情報として含めるが、順序性・証跡の根拠としては扱わない。

```text
AuditRecord { sequence : u64, event_id : varint, prev_hash : bytes(32), timestamp : u64, reason : ReasonCode, ... }
```

チェーン先頭ハッシュは定期的に外部へアンカリングする(sequenceとhashのみ)。

## 2.16 TCPフォールバック用ALPN

`sardp-tcp/1`。詳細はPart 5。

---

# Part 3. Security Model

- 通信: TLS 1.3、鍵交換 X25519、暗号 ChaCha20-Poly1305 または AES-256-GCM。
- 認証: v0.3必須はPUBLIC_KEY/PASSKEYのみ。チャネルバインディング必須(2.3節)。チャレンジは一度限り、試行回数上限あり。
- 認可: デフォルト拒否。`PermissionSet` は常に実効許可状態。
- 0-RTT: 初期版は無効。将来許可する場合も読み取り専用操作のみ。
- 再接続: 短命・一回限りのトークン、アトミックな消費。
- ファイル転送: 論理パス+ポリシー解決+不透明ハンドル方式。
- プロセス分離: ネットワーク処理(低権限)/ポリシー判定(特権)/キャプチャ・入力注入(ユーザーセッション内)。
- 鍵管理: 秘密鍵はTPM / Secure Enclave / OSキーチェーン。

### リレーの暗号境界(レビュー指摘#15)

自組織運用リレーを用いる場合、以下をMUSTとする。

- リレーはSARDPの復号鍵を一切保持しない。
- リレーは認証情報・画面内容・入力内容を平文で取得できない(TLS/QUICはエンドポイント間でのみ終端する)。
- リレーが観測可能なのは、接続先識別情報・通信量・タイミングなどのメタデータのみ。
- リレーは接続確立のためのメタデータ中継のみを行う。
- 自組織運用リレーは組織スコープの証明書(`RelayIdentity`)を提示し、クライアントはこれを検証・ピン留めする。匿名の共有リレーへの自動フォールバックは行わない(DR-013)。

---

# Part 4. State Machines

本Partは実装者がコードに直接落とせる粒度で、SARDPの状態機械を規定する。各節は状態ごとに「許可されるメッセージ」「禁止されるメッセージとその結果」「遷移条件」「タイムアウト」「失敗時のReasonCode」を示す。

### 4.0 表記規則

- ReasonCodeは `DOMAIN.番号 名前` の形式で示す(2.8節の `domain` 値: 1=AUTH, 2=POLICY, 3=TRANSPORT, 4=PROTOCOL, 5=OS)。具体的な番号割り当ては4.8節Error Handling Matrixで確定する。
- タイムアウトのうち、既存節(2.x)で既に数値が確定しているものはその値を引用する。4.7節のタイムアウトモデルにある値は、本改訂で数値的根拠とともに確定させたものである(サブエージェントによる妥当性検証を経ている。検証過程は4.7節末尾の注記を参照)。なお数値未確定の項目が2件のみ残っており、4.9節に一覧化する。
- 「許可されるメッセージ」に無い `type` を受信した場合、既定の結果は2.1.1節のignorable flag処理に従う。ignorable flagが立っていない核心メッセージが状態に反して送られた場合はMUSTでプロトコル違反として扱う(具体的ReasonCodeは4.8節)。

## 4.1 Connection State Machine

論理セッション全体の状態。実際のメッセージは `control` ストリーム(TCPフォールバック時は control 用コネクション)上でやり取りする。

```text
Handshaking --(ClientHello/ServerHello交換完了)--> Authenticating
Authenticating --(accepted_combinationsのいずれかを完全に満たしAuthResult: OK送出)--> Authenticated
Authenticating --(AuthResult: DENIED / 試行回数上限[3回])--> Closing
Authenticated --(いずれか1つのVideoStream ChannelがLiveに到達)--> Active   [DR-024]
Active --(IDLE_TIMEOUT[45秒]: KeepAlive/任意メッセージの不在)--> Suspended
Active --(SessionClose受信・送信 / 強制切断 / MAX_SESSION_DURATION[既定24時間]超過)--> Closing
Suspended --(Reconnection State Machine: ReconnectAccepted、4.6節)--> Active
Suspended --(RECONNECT_GRACE_PERIOD[300秒]超過)--> Closed  ※Closingを経由せず直接Closed
Handshaking --(HANDSHAKE_TIMEOUT[10秒])--> Closing
Authenticating --(AUTH_TIMEOUT[60秒、試行ごとにリセット])--> Closing
Authenticated --(SESSION_SETUP_TIMEOUT[15秒])--> Closing
任意の状態 --(致命的プロトコル違反)--> Closing
Closing --(切断処理完了[既定2秒の猶予])--> Closed
```

| 状態 | 許可されるメッセージ(control以外は不可) | 禁止 → 結果 | タイムアウト | 失敗ReasonCode |
|---|---|---|---|---|
| **Handshaking** | `StreamPrologue(kind=control)`、`ClientHello`、`ServerHello` のみ | 他のkindのストリーム開始 → 即切断 | `HANDSHAKE_TIMEOUT` = 10秒(QUIC/TCPフォールバック双方に一律適用) | `TRANSPORT.HANDSHAKE_TIMEOUT` |
| **Authenticating** | `AuthPubkey`、`AuthPasskeyAssertion`、`AuthChallengeRenew`(server→client)。`PASSWORD`/`TOTP`系メッセージはDR-020によりv0.3未定義のためMUST NOTで送信、受信側はプロトコル違反として扱う | control以外のストリーム開始、`DisplayConfig`等の設定メッセージ → 切断 | `AUTH_TIMEOUT` = 60秒(**試行ごとにリセット**。`AuthChallengeRenew`送出のたびに再起算)。試行回数上限 = 3回(2.3節、既定) | `AUTH.AUTH_TIMEOUT` / `AUTH.TOO_MANY_ATTEMPTS` / `AUTH.SIGNATURE_INVALID` |
| **Authenticated** | `DisplayConfig`、`DisplayCapabilities`、`ActiveMonitor`、`TimeSyncRequest/Response`、`KeepAlive`、`PermissionUpdate`。**持続系ストリーム**(video, audio_playback, audio_capture, input, feedback)の開設・稼働は許可。**一時系ストリーム**(clipboard, file)はMUST NOTで開設不可 | 一時系ストリームの開設 → `PROTOCOL.UNEXPECTED_MESSAGE`、当該ストリームを拒否 | `SESSION_SETUP_TIMEOUT` = 15秒(VIDEO_CONFIGURING_TIMEOUTの5倍の余裕) | `PROTOCOL.SESSION_SETUP_TIMEOUT` |
| **Active** | すべて許可(`ClientHello`/`ServerHello`/`AuthChallengeRenew`を除く) | `ClientHello`/`ServerHello`の再送 → `PROTOCOL.UNEXPECTED_MESSAGE` | `IDLE_TIMEOUT` = 45秒(`KEEPALIVE_INTERVAL`[15秒、2.9節]の3倍)。`MAX_SESSION_DURATION` = 既定24時間、ポリシーで無制限化可(3.節参照) | `TRANSPORT.IDLE_TIMEOUT` / `POLICY.MAX_SESSION_DURATION_EXCEEDED` |
| **Suspended** | 接続が存在しないため本状態自体にはメッセージ往来がない。新規コネクションの受け入れは4.6節Reconnection SMに従う | — | `RECONNECT_GRACE_PERIOD` = 300秒 | `TRANSPORT.RECONNECT_TIMEOUT` |
| **Closing** | `SessionClose` のみ | それ以外は無視してよい(MAY) | 既定2秒の切断猶予(SHOULD) | — |

`MAX_SESSION_DURATION`(既定24時間)は在席・対話的セッションを想定した推奨既定値である。無人・サービスアカウント用途では、セッションが強制的に切れることが業務継続の妨げになるため、運用ポリシーとして無制限(0/オフ)に設定することをSHOULDとする。
| **Closed** | なし(終端状態) | — | — | — |

## 4.2 Stream Lifecycle State Machine(汎用)

全ストリーム種別に共通する骨格。`video`(4.3節)はこれに加えて固有の内部状態を持つ。

| kind | 分類 | 開設可能な状態(4.1節) | Closingの契機 |
|---|---|---|---|
| control | 持続 | Handshaking(自身が起点) | Connection SMのClosing |
| input, feedback | 持続 | Authenticated以降 | Connection SMのClosing |
| video, audio_playback, audio_capture | 持続 | Authenticated以降 | Connection SMのClosing、または4.3節のInstance再オープン |
| clipboard | 一時(request_idごと) | Active | 交換完了(`ClipboardData`/`ClipboardError`送出後) |
| file | 一時(file_handleごと) | Active | `FileTransferComplete`/`FileTransferError`送出後 |

```text
Opening --(StreamPrologue受信・magic/kind検証)--> PrologueVerified
Opening --(magic不一致 / 未知kind / 4.1節で禁止された時点でのkind)--> Rejected --> Closed
PrologueVerified --(当該kindの最初の正当なEnvelope受信)--> Active
PrologueVerified --(検証失敗)--> Rejected --> Closed
Active --(kindごとの終了条件、上表) --> Closing --> Closed
Active --(一時系ストリームの無通信超過。clipboardは`CLIPBOARD_RESPONSE_TIMEOUT`[5秒]、fileは`FILE_TRANSFER_STALL_TIMEOUT`[30秒])--> Closing --> Closed
```

- `Opening→PrologueVerified` の検証失敗時のReasonCode: `PROTOCOL.PROLOGUE_MAGIC_MISMATCH`(magic不一致)、`PROTOCOL.UNKNOWN_STREAM_KIND`(未知kind)、`PROTOCOL.WRONG_INITIATOR`(2.2.1表の初期化側と異なる側が開いた)。
- 一時系ストリームの `STREAM_IDLE_TIMEOUT` は、独立した値を新設せず既存の値を流用する: `clipboard` は `CLIPBOARD_RESPONSE_TIMEOUT`(5秒、2.7節)、`file` は `FILE_TRANSFER_STALL_TIMEOUT`(30秒)を適用する。

## 4.3 VideoStream State Machine

映像は2層で構成する。**Channel**(モニター単位、世代をまたいで持続する概念)と、**Instance**(QUICストリーム1本=1世代に対応する実体)である。v0.3旧版のPart 4はこの2層を1つの図に混在させていたため、本改訂で分離する。

### 4.3.1 Channel State Machine(モニターごと、`context_id`に対応)

```text
Initializing --(最初のInstanceがStreamingに到達)--> Live
Live --(現在のInstanceがCongested→Closed(Reset)、新Instance生成)--> Recovering
Recovering --(新InstanceがStreamingに到達)--> Live
Recovering --(VIDEO_RECOVERY_TIMEOUT[5秒]超過)--> Recovering
    ※バックオフ後に新Instance(generation+1)で再試行。上限を設けず無期限に継続する(DR-030)。
      バックオフ間隔は 1s, 2s, 4s, 8s, 16s, 30s(以降30s固定)と指数的に増加させ、
      Connection SMがActiveである限り試行を止めない。恒久的な「諦め」状態は設けない。
Live --(ActiveMonitorが他モニターへ移動)--> Paused
Paused --(ActiveMonitorが復帰)--> Live
任意の状態 --(SessionClose / DisplayConfigからの当該monitor_id削除)--> Closed
```

Paused状態では、下位のInstanceはStreamingのまま維持し、エンコーダが送出レート・解像度のみを下げる(generationは変えない)。Recoveringは必ずInstanceのgeneration増分を伴う。

### 4.3.2 Instance State Machine(QUICストリーム1本、1世代)

```text
Created --(StreamPrologue送出)--> Configuring
Configuring --(VideoStreamGeneration + EncoderConfig送出)--> Configuring   ※同一状態内の内部遷移
Configuring --(当該generationの最初のIDR送出)--> Streaming
Streaming --(client_queue_delay_us - baseline > VIDEO_RATE_REDUCE_THRESHOLD_DELTA[100ms])--> Congested
Congested --(baselineからのdeltaがVIDEO_RATE_REDUCE_THRESHOLD_DELTA未満に500ms[ヒステリシス]以上留まる)--> Streaming
Congested --(client_queue_delay_us - baseline > MAX_VIDEO_QUEUE_DURATION_DELTA[300ms]が3回連続 または app_send_queue_bytes > MAX_VIDEO_QUEUE_BYTES[4MiB])--> Closed(Reset)
Configuring --(VIDEO_CONFIGURING_TIMEOUT[3秒]超過)--> Closed(Failed)
任意の状態 --(SessionClose / 接続断)--> Closed(Normal)
```

| 状態 | 許可されるメッセージ(このInstanceのストリーム上、server→clientのみ) | 禁止 → 結果 |
|---|---|---|
| Created | (StreamPrologueのみ、Envelopeはまだ無い) | Envelope送出 → 内部エラー(実装バグ) |
| Configuring | `VideoStreamGeneration`(1回、最初)、`EncoderConfig`(1回、直後) | `VideoFrame`をIDR受領前に送る → `PROTOCOL.UNEXPECTED_MESSAGE` |
| Streaming / Congested | `VideoFrame`(IDR・非IDR、非IDRはBフレーム禁止によりPフレームのみ) | 同一Instance上での`VideoStreamGeneration`/`EncoderConfig`の再送 → `PROTOCOL.UNEXPECTED_MESSAGE`(世代変更は新Instanceでのみ行う) |
| Closed(*) | なし | — |

**重要な区別**: 本節の `Congested → Closed(Reset)` は2.10節で規定した常時有効なコア機構であり、閾値はベースライン相対のdeltaで100ms/300ms(確定値)である。2.14節の「Datagram縮退条件」(150ms×3連続 または decoded_frame_age>200ms)は**第2段階のQUIC Datagram機能専用の別基準**であり、本状態機械には適用しない。両者を同一の遷移条件として扱わないこと。

クライアント側規範(MUST、2.10節より再掲):

- 新しい `generation` を含む `VideoStreamGeneration` を受信したら、それ未満のgenerationに属する未表示フレームを破棄する。
- `EncoderConfig` と当該generationの最初のIDRを受信するまで表示を更新しない。
- ストリームのリセット通知を受けた際、そのストリームの部分バッファ済みフレームを破棄する。

失敗ReasonCode: `PROTOCOL.VIDEO_CONFIGURING_TIMEOUT`(Configuring超過)、`PROTOCOL.FRAME_LENGTH_MISMATCH`(`payload_len`とEnvelope長の不一致)、`PROTOCOL.GENERATION_MISMATCH`(クライアントが既知の最新generationより古いgenerationのフレームを、新generation受信後に処理しようとした場合の内部検出用)、`OS.DECODE_ERROR`(クライアントが`KeyframeRequest{reason=DECODE_ERROR}`を送った場合、サーバー側ログ用)。

## 4.4 Input State Machine

`input` ストリームはQUICストリームの順序保証により、`event_id` は受信順に単調増加することが保証される。実装は到着順の検証・並べ替えを行う必要はない。入力の処理は VideoStream の状態(Congested等)から独立しており、映像の劣化中も通常どおり処理する。

### 4.4.1 IMEモードSM(セッションごとに1つ、既定 `CLIENT_SIDE`)

```text
CLIENT_SIDE --(ImeModeChange{mode=REMOTE_SIDE, effective_after_event_id=N})--> REMOTE_SIDE
REMOTE_SIDE --(ImeModeChange{mode=CLIENT_SIDE, effective_after_event_id=N})--> CLIENT_SIDE
```

`event_id <= N` はMUSTで旧モードとして処理し、`event_id > N` はMUSTで新モードとして処理する。`ImeModeChange` は`input`ストリームの方向性(client→server単方向、2.2.1節)により **client起点のみ** であり、サーバーからのモード変更要求はv0.3では未定義(将来課題、4.9節)。

| モード | 許可 | 禁止 → 結果 |
|---|---|---|
| CLIENT_SIDE | `TextInput`、`ImeComposition`。**IME変換の対象となる物理キーの`KeyEvent`は、変換が進行中の間MUST NOTで送信しない**(DR-025、二重入力防止)。変換に無関係なキー・マウスイベントは通常どおり送信する。 | 進行中の変換に関わる`KeyEvent`の送信 → 受信側は無視するがログレベルの逸脱として扱ってよい(実装は検出を強制されない) |
| REMOTE_SIDE | すべての`KeyEvent`を生のまま送信(IME関連キーも含む) | `TextInput`/`ImeComposition`の送信 → `PROTOCOL.UNEXPECTED_MESSAGE` |

### 4.4.2 キー押下状態の不変条件

サーバーは押下中のキー・ボタンの集合を追跡する(`Idle` ⇄ `Down`、キー/ボタンごと)。

```text
Idle --(KeyEvent/MouseButton: down=true)--> Down
Down --(KeyEvent/MouseButton: down=false)--> Idle
Down --(inputストリームのClosing、またはConnection SMのSuspended/Closing遷移)--> Idle
    ※MUSTでサーバーが当該キー/ボタンの release(down=false)イベントを合成し、
      OSへ注入する(2.12節の安全処理)
```

## 4.5 Permission State Machine

`PermissionSet` の各ビットは独立したFSMを持つ。初期状態は `AuthResult.granted_permissions` に従う。

```text
NotGranted --(PermissionUpdate: ビットがgranted_permissionsに追加)--> Granted
Granted --(PermissionUpdate: ビットがgranted_permissionsから削除 かつ immediate_revokeに含む)--> NotGranted
Granted --(PermissionUpdate: ビットがgranted_permissionsから削除 かつ immediate_revokeに含まない)--> Draining
Draining --(該当する進行中の操作がすべて完了)--> NotGranted
Draining --(新規操作の試行)--> [その場で拒否、状態はDrainingのまま]
Draining --(強制終了タイムアウトの超過。FILE_*系は`FILE_TRANSFER_STALL_TIMEOUT`[30秒]、CLIP_*系は`CLIPBOARD_RESPONSE_TIMEOUT`[5秒]を流用)--> NotGranted(該当ストリームを強制終了)
```

| 状態 | 許可される操作 | 禁止される操作 → 結果 |
|---|---|---|
| Granted | 当該ビットに対応する新規操作の開始(例: `FILE_UP`→`FileTransferRequest{direction=UPLOAD}`) | — |
| Draining | 既に進行中の操作の継続のみ | 新規操作の開始 → `POLICY.PERMISSION_REVOKED`(該当メッセージの応答として`FileTransferReject`/`ClipboardError`等) |
| NotGranted | なし | 新規操作の開始 → `POLICY.PERMISSION_DENIED` |

`immediate_revoke` に典型的に含める権限は `INPUT_KEYBOARD`/`INPUT_MOUSE`/`ADMIN`、含めないのは `FILE_UP`/`FILE_DOWN`/`CLIP_READ`/`CLIP_WRITE` を想定する(2.5節、SHOULD、強制ではない)。

## 4.6 Reconnection State Machine

```text
Suspended
  --(新規コネクションのcontrolストリームでStreamPrologue直後にSessionReauthenticate{RECONNECT}を受信)--> ReconnectAttempt
ReconnectAttempt
  --(reconnect_tokenをアトミックに検証・消費: session_id一致、user_id一致、
     未消費、RECONNECT_TOKEN_EXPIRY[300秒]未満)--> ReconnectAccepted
ReconnectAttempt
  --(検証失敗のいずれか)--> ReconnectRejected
ReconnectAccepted
  --(新しいAuthResult{OK}送出。旧reconnect_token失効、新トークン発行。
     video/audio/input/feedbackの各ストリームを新コネクション上でMUSTで再オープン)--> [Connection SM: Active]
ReconnectRejected
  --(AuthResult{DENIED}送出)--> [接続切断、Connection SMには遷移しない]
Suspended
  --(RECONNECT_GRACE_PERIOD[300秒]超過、新規コネクションなし)--> [Connection SM: Closed]
```

決定事項:

- **generationはセッションを通じて単調増加を継続し、再接続時にリセットしない**(DR-026)。`frame_id`/`event_id` 等と同様、セッションスコープの単調カウンタとして扱う。
- 再接続後の映像は、旧接続で使っていたInstanceを再利用できない(QUICストリームはコネクションに束縛される)ため、MUSTで新Instanceを`Created`から開始し、`generation+1`で自己完結IDRから再開する。
- ClientHello/ServerHelloの再交換は行わない。Capability・DisplayCapabilitiesは元のセッションのものを維持する(再申告は行わない)。異なるデバイス/設定での再接続時にこれが適切かは未決定(4.9節)。
- `RECONNECT_TOKEN_EXPIRY` と `RECONNECT_GRACE_PERIOD` は同一値(300秒)とする。トークンはサーバー側で状態管理される不透明な値であり、猶予期間より長く生かしておく理由がないため、2つの独立した数値を持たせない。

失敗ReasonCode: `AUTH.RECONNECT_TOKEN_INVALID` / `AUTH.RECONNECT_TOKEN_EXPIRED` / `AUTH.RECONNECT_TOKEN_ALREADY_CONSUMED`。

## 4.7 Timeout and Error Model

| タイマー | スコープ | 値 | 起点 | 満了時の遷移 | ReasonCode |
|---|---|---|---|---|---|
| HANDSHAKE_TIMEOUT | Connection | **10秒**(確定) | コネクション確立 | Handshaking→Closing | TRANSPORT.HANDSHAKE_TIMEOUT |
| AUTH_TIMEOUT | Connection | **60秒、試行ごとにリセット**(確定) | ServerHello送出、または各AuthChallengeRenew送出 | Authenticating→Closing | AUTH.AUTH_TIMEOUT |
| AUTH試行回数上限 | Connection | 3回(確定、2.3節) | 認証失敗ごとに加算 | Authenticating→Closing | AUTH.TOO_MANY_ATTEMPTS |
| SESSION_SETUP_TIMEOUT | Connection | **15秒**(確定) | Authenticated突入 | Authenticated→Closing | PROTOCOL.SESSION_SETUP_TIMEOUT |
| KEEPALIVE_INTERVAL | Connection | **15秒**(確定、2.9節) | 周期 | (周期送信、タイムアウトではない) | — |
| IDLE_TIMEOUT | Connection | **45秒**(確定。KEEPALIVE_INTERVALの3倍) | 最終受信からの経過 | Active→Suspended | TRANSPORT.IDLE_TIMEOUT |
| MAX_SESSION_DURATION | Connection | **既定24時間、ポリシーで無制限化可**(確定) | Active突入 | Active→Closing | POLICY.MAX_SESSION_DURATION_EXCEEDED |
| RECONNECT_GRACE_PERIOD | Connection | **300秒**(確定) | Suspended突入 | Suspended→Closed | TRANSPORT.RECONNECT_TIMEOUT |
| RECONNECT_TOKEN_EXPIRY | Session | **300秒、RECONNECT_GRACE_PERIODと同値**(確定) | トークン発行 | トークン無効化 | AUTH.RECONNECT_TOKEN_EXPIRED |
| VIDEO_CONFIGURING_TIMEOUT | Instance | **3秒**(確定) | Configuring突入 | Instance→Closed(Failed) | PROTOCOL.VIDEO_CONFIGURING_TIMEOUT |
| VIDEO_QUEUE_BASELINE_WINDOW | Instance/Channel | **10秒**(確定、2.10節) | 継続監視(ローリングウィンドウ) | baseline値の更新 | — |
| VIDEO_RATE_REDUCE_THRESHOLD_DELTA | Instance | **baselineから100ms**(確定、2.10節、絶対値ではなくdelta) | 継続監視 | Streaming→Congested | — |
| Congested→Streaming ヒステリシス | Instance | **500ms**(確定) | deltaが閾値未満に復帰後の継続監視 | Congested→Streaming | — |
| MAX_VIDEO_QUEUE_DURATION_DELTA | Instance | **baselineから300ms**(確定、2.10節、絶対値からdeltaへ訂正) | 継続監視(3回連続) | Congested→Closed(Reset) | — |
| MAX_VIDEO_QUEUE_BYTES | Instance | 4MiB(確定、2.10節) | 継続監視 | Congested→Closed(Reset) | — |
| VIDEO_RECOVERY_TIMEOUT | Channel | **5秒/回、無期限バックオフ再試行**(確定。DR-030) | Recovering突入 | Recovering→Recovering(再試行、バックオフ1s/2s/4s/8s/16s/30s上限) | TRANSPORT.VIDEO_RECOVERY_TIMEOUT |
| TRANSPORT_FEEDBACK_INTERVAL | Feedback | 100ms(確定、2.14節) | 周期 | (周期送信、タイムアウトではない) | — |
| AUDIO_SYNC_FEEDBACK_INTERVAL | Audio | 2秒(確定、2.13節) | 周期 | 同上 | — |
| AUDIO_JITTER_SKIP_THRESHOLD | Audio | 200ms(確定、2.13節) | 継続監視 | 音声フレームskip-ahead | — |
| CLIPBOARD_RESPONSE_TIMEOUT | Clipboard交換 | 5秒(確定、2.7節、SHOULD) | ClipboardRequest送出 | 要求側がタイムアウト扱い | — |
| FILE_TRANSFER_STALL_TIMEOUT | File転送 | **30秒**(確定) | 最終FileChunk受信 | ストリーム強制終了 | TRANSPORT.STREAM_STALL_TIMEOUT |
| Closing猶予 | Connection | **2秒**(確定) | SessionClose送出 | Closing→Closed | — |

一時系ストリーム(clipboard/file)の`STREAM_IDLE_TIMEOUT`は独立値を持たず、`CLIPBOARD_RESPONSE_TIMEOUT`と`FILE_TRANSFER_STALL_TIMEOUT`にそれぞれ統合した(4.2節)。

## 4.8 Error Handling Matrix

| 条件 | 検出箇所 | ReasonCode | アクション |
|---|---|---|---|
| StreamPrologue.magic不一致 | Stream Lifecycle(Opening) | PROTOCOL.3 PROLOGUE_MAGIC_MISMATCH | 当該ストリームを異常終了 |
| 未知のkind | Stream Lifecycle(Opening) | PROTOCOL.4 UNKNOWN_STREAM_KIND | 当該ストリームを異常終了 |
| 2.2.1表と異なる側がストリームを開いた | Stream Lifecycle(Opening) | PROTOCOL.5 WRONG_INITIATOR | 接続を切断(重大なプロトコル違反) |
| 未認証状態でcontrol以外を開いた | Connection(Handshaking/Authenticating) | PROTOCOL.1 UNEXPECTED_MESSAGE | 接続を切断 |
| ignorable flag=0の未知type | Envelope解析 | PROTOCOL.2 UNKNOWN_CORE_MESSAGE | 当該ストリームを異常終了(controlなら接続切断) |
| Envelope.lengthがストリーム上限超過 | Envelope解析 | PROTOCOL.1 UNEXPECTED_MESSAGE | 当該ストリームを異常終了 |
| 認証署名検証失敗 | Authenticating | AUTH.3 SIGNATURE_INVALID | `AuthChallengeRenew`送出、試行回数加算 |
| auth_challengeの再利用 | Authenticating | AUTH.4 CHALLENGE_REUSE | 接続を切断 |
| AUTH試行回数上限到達 | Authenticating | AUTH.2 TOO_MANY_ATTEMPTS | 接続を切断 |
| reconnect_token不正/期限切れ/消費済み | Reconnection SM | AUTH.5/6/7 | `AuthResult{DENIED}`後に切断、Connection SMには遷移しない |
| VideoFrame.payload_lenとEnvelope長の不一致 | VideoStream Instance | PROTOCOL.8 FRAME_LENGTH_MISMATCH | 当該Instanceを異常終了、Channelはgeneration+1で再開 |
| Configuring状態でIDR未達のままVIDEO_CONFIGURING_TIMEOUT | VideoStream Instance | PROTOCOL.7 VIDEO_CONFIGURING_TIMEOUT | Instance→Closed(Failed)、Channel→Recovering再試行 |
| クライアントからのKeyframeRequest{DECODE_ERROR} | VideoStream(feedback経由) | OS.2 DECODE_ERROR | サーバーはgeneration+1でストリーム再開を検討(MAY) |
| FileChunkのoffset重複 | File転送 | PROTOCOL.10 FILE_CHUNK_OVERLAP | `FileTransferError`送出、転送中断 |
| FileChunkがresolved_size超過 | File転送 | PROTOCOL.11 FILE_CHUNK_OUT_OF_RANGE | `FileTransferError`送出、転送中断 |
| 穴のある状態でFileTransferComplete受理 | File転送 | PROTOCOL.12 FILE_INCOMPLETE_TRANSFER | `FileTransferError`送出、転送失敗扱い |
| checksum不一致 | File転送 | PROTOCOL.13 FILE_CHECKSUM_MISMATCH | `FileTransferError`送出、転送失敗扱い |
| virtual_pathのポリシー/サンドボックス違反 | File転送(ポリシー層) | POLICY.5 FILE_POLICY_REJECTED | `FileTransferReject`送出 |
| 権限を持たない操作の要求 | Permission SM | POLICY.1 PERMISSION_DENIED | 該当メッセージの拒否応答 |
| Draining中の新規操作要求 | Permission SM | POLICY.2 PERMISSION_REVOKED | 該当メッセージの拒否応答 |
| クリップボード形式サイズ超過 | Clipboard交換 | POLICY.6 CLIPBOARD_FORMAT_TOO_LARGE | `ClipboardError`送出 |
| REMOTE_SIDEモードでTextInput/ImeComposition受信 | Input SM | PROTOCOL.1 UNEXPECTED_MESSAGE | 当該メッセージを破棄、ログに記録 |
| MAX_SESSION_DURATION超過 | Connection(Active) | POLICY.4 MAX_SESSION_DURATION_EXCEEDED | `SessionClose`送出、Closing遷移 |
| 管理者による強制切断 | Connection(Active) | POLICY.3 FORCED_DISCONNECT | `SessionClose`送出、Closing遷移 |

### 4.8.1 ReasonCode 一覧(正式版、DR-034)

上表と4.1/4.6/4.7節で名前のみ参照していたコードを合わせ、`domain.code` の割り当てをここに一元化する。以降、本書内の全参照はこの表を正とする。

**AUTH(domain=1)**

| code | 名前 |
|---|---|
| 1 | AUTH_TIMEOUT |
| 2 | TOO_MANY_ATTEMPTS |
| 3 | SIGNATURE_INVALID |
| 4 | CHALLENGE_REUSE |
| 5 | RECONNECT_TOKEN_INVALID |
| 6 | RECONNECT_TOKEN_EXPIRED |
| 7 | RECONNECT_TOKEN_ALREADY_CONSUMED |

**POLICY(domain=2)**

| code | 名前 |
|---|---|
| 1 | PERMISSION_DENIED |
| 2 | PERMISSION_REVOKED |
| 3 | FORCED_DISCONNECT |
| 4 | MAX_SESSION_DURATION_EXCEEDED |
| 5 | FILE_POLICY_REJECTED |
| 6 | CLIPBOARD_FORMAT_TOO_LARGE |

**TRANSPORT(domain=3)**

| code | 名前 |
|---|---|
| 1 | HANDSHAKE_TIMEOUT |
| 2 | IDLE_TIMEOUT |
| 3 | RECONNECT_TIMEOUT |
| 4 | VIDEO_RECOVERY_TIMEOUT |
| 5 | STREAM_STALL_TIMEOUT |

**PROTOCOL(domain=4)**

| code | 名前 |
|---|---|
| 1 | UNEXPECTED_MESSAGE |
| 2 | UNKNOWN_CORE_MESSAGE |
| 3 | PROLOGUE_MAGIC_MISMATCH |
| 4 | UNKNOWN_STREAM_KIND |
| 5 | WRONG_INITIATOR |
| 6 | SESSION_SETUP_TIMEOUT |
| 7 | VIDEO_CONFIGURING_TIMEOUT |
| 8 | FRAME_LENGTH_MISMATCH |
| 9 | GENERATION_MISMATCH |
| 10 | FILE_CHUNK_OVERLAP |
| 11 | FILE_CHUNK_OUT_OF_RANGE |
| 12 | FILE_INCOMPLETE_TRANSFER |
| 13 | FILE_CHECKSUM_MISMATCH |

**OS(domain=5)**

| code | 名前 |
|---|---|
| 2 | DECODE_ERROR |

(1, 3の`CAPTURE_FAILURE`/`INPUT_INJECTION_FAILURE`は本文中で未使用のため、必要になった時点で改めて割り当てる。2番から始まる欠番は元の設計メモに合わせたもので、詰め直す実益がないためそのままにしてある。)

## 4.9 未決定事項(本改訂で残る2件)

前回の改訂で洗い出した10項目のうち、数値的な決定が必要だった8項目はすべて4.7節で確定した(値の根拠・サブエージェントによる妥当性検証はチャット履歴および各節の注記を参照)。数値ではなく挙動そのものが未決定の残り2項目のみ、引き続き未決定事項として一覧化する。

1. 再接続時にCapability/DisplayCapabilitiesを再申告するかどうか(4.6節)。現状は元セッションの設定を維持する前提だが、異なるデバイス・異なる能力での再接続を許容する場合は必須の検討事項。
2. サーバー起点のIMEモード変更要求を将来サポートするか(4.4.1節)。現状 `ImeModeChange` はclient起点のみで、`input` ストリームの単方向性(2.2.1節)によりサーバーからの要求はプロトコル上表現できない。

---

# Part 5. Transport Bindings

## 5.1 QUICバインディング(標準)

ALPN `sardp/1`。Part 2の全メッセージはQUICストリーム上でStreamPrologue+Envelopeとしてやり取りする。輻輳制御はQUICスタックの既定に任せ、アプリ層(エンコーダの目標ビットレート等)はPart 2.10のバックプレッシャ機構で制御する。

## 5.2 TCPフォールバックバインディング

ALPN `sardp-tcp/1`。QUICのstream・flow control・congestion controlはTCPに存在しないため、muxレイヤーは作らず、論理チャネルごとに個別のTLS 1.3+TCPコネクションを張る。コネクション種別と初期化側は2.2.1節の表に準じる(video/audioはモニター・方向ごとに複数コネクション)。

各コネクションはTLSハンドシェイク直後に以下を送り、認証済みセッションへ束縛する。

```text
ChannelBind {
    session_id : bytes(16)
    kind       : u8
    context_id : varint
    proof      : bytes(32)   // TLS-Exporter("EXPORTER-SARDP-channelbind-v1",
                              //   session_id || kind || context_id, 32)
}
```

`proof` の検証に失敗した接続はMUSTで即切断する。クライアントはQUIC(UDP)での接続を既定で試み、確立に失敗した場合にのみTCPフォールバックへ切り替える。

TCPフォールバックにおいてもgeneration管理(2.10節)は同様に適用され、映像コネクションの再確立時にはgenerationを1つ増やして新しいTCPコネクションを張る。

---

# Part 6. Media Model

- 映像: 単一H.264エンコーダ、Bフレーム禁止、モニターごとの独立ストリーム+generation管理、QPマップによる文字領域優遇。
- 音声: Opus固定、方向別ストリーム、クロックドリフト補正(2.13節)。
- カーソル: クライアント側描画優先、除外不可環境はサーバー側焼き込みへ切替。

エンコーダ能力Tier(参考、ワイヤには出さないサーバー内部区分):

| Tier | 内容 |
|---|---|
| 1 | ハードウェア4:4:4 + ROI/QPマップ |
| 2 | ハードウェア4:2:0 + QPマップ + 文字用補助レイヤー |
| 3 | ハードウェア4:2:0のみ。静止領域を高品質フレームで更新 |
| 4 | ソフトウェアエンコード |

---

# Part 7. OS Integration

- **Windows**: UAC・ログイン画面はSecure Desktop上。無人接続にはSYSTEMサービス+ユーザーセッション内エージェントが必須。DRM保護コンテンツは黒画面になり得る。
- **macOS**: ScreenCaptureKitを使用。
- **Linux X11**: XDamage等でダーティリージョンを取得。
- **Linux Wayland**: 独自コンポジタは作らず、ディストリビューション標準のリモートデスクトップポータルに認証・アクセス制御・監査ログ層として連携する(DR-012)。
- **共通**: フレーム取得はOSのフレーム到着イベント駆動。変化がなければ映像を送らずkeepaliveのみ維持する。

USBリダイレクト: デバイスクラス単位の許可制+用途プリセット。全面対応は将来課題(Appendix B)。

---

# Part 8. Implementation Requirements

- Envelopeパーサーは手書きの最小実装とし、MUSTでファジング(cargo-fuzz / libFuzzer相当)対象に含める。
- メッセージ本体はスキーマ生成コード(FlatBuffers等、実装が選択)で管理する。
- 測定ハーネスを実装前に用意する: フレーム生成時刻の画面埋め込み、E2E遅延自動計測、`tc netem` によるネットワーク再現。
- 目標値: LAN glass-to-glass 50ms以下、WAN 150ms以下。
- 最小プロトタイプ: ダミー映像源→H.264ソフトエンコード→QUIC 1ストリーム(モニター1枚分)→デコード表示。OSキャプチャ・ハードウェアエンコードは後から差し込む。

---

# Appendix A. Design History & Decision Records

詳細な議論の全文は `sardp-schema-v0.1.md` を参照。

| DR | 決定 | 棄却した案 |
|---|---|---|
| DR-001 | 映像は初期版で全信頼性ストリーム | キーフレームのみreliable、P/Bはunreliable(参照チェーン破壊) |
| DR-002 | QUIC単一コネクションを標準 | 制御=TCP、映像=QUICの2系統 |
| DR-003 | 単一動画エンコーダ+QPマップ | タイル差分+動画のハイブリッド |
| DR-004 | カーソルはクライアント側描画、除外不可環境はサーバー焼き込みへフォールバック | 二重カーソル表示の許容 |
| DR-005 | 認証はTLSエクスポーターによるチャネルバインディング必須 | 署名対象未定義の公開鍵認証 |
| DR-006 | モニターごとに独立映像ストリーム | 単一ストリームへの多重化 |
| DR-007 | バックプレッシャは送信元ドロップ+ストリーム再開 | RESET_STREAMによる個別フレーム破棄 |
| DR-008 | TCPフォールバックはチャネルごとの個別コネクション | QUIC streamモデルのTCP上への模倣 |
| DR-009 | ファイル転送はvirtual_path+ポリシー解決+不透明ハンドル | 生のOSパスをワイヤに直接載せる方式 |
| DR-010 | PermissionUpdateはimmediate_revokeで権限ごとに即時/段階を分離 | 一律のeffective_immediately bool |
| DR-011 | DisplayCapabilitiesはデコード関連のみ | エンコーダ機能(qp_map等)をクライアント申告に含める |
| DR-012 | Waylandは独自コンポジタを作らずディストリ標準ポータルに連携 | 自前の仮想ディスプレイ+wlrootsコンポジタ |
| DR-013 | 直接接続優先+自組織運用リレー | 匿名の共有リレーへの自動フォールバック |
| DR-014 | Envelope.typeをignorable flag+core/experimental範囲に分割 | フラットなtype空間 |
| DR-015 | StreamPrologueによるストリーム自己識別 | QUIC stream IDそのものに意味を持たせる |
| DR-016 | バージョニングはCapability交渉基本、非互換変更のみALPNを上げる | 全変更をALPNバージョンで管理 |
| DR-017 | 映像ストリームにgeneration番号を導入し、リセット後のフレームを明示的に区別 | generation無しでストリーム再開のみ行う(新旧フレームの取り違えリスク) |
| DR-018 | ストリームごとに初期化側・方向性を固定(2.2.1節) | 全ストリームを画一的に「双方向」とする曖昧な定義 |
| DR-019 | Bフレームを禁止し、frame_id順=デコード順=表示順に統一 | decode_order/display_orderを別途追加してBフレームを許容 |
| DR-020 | v0.3必須の認証方式はPUBLIC_KEY/PASSKEYのみ、PASSWORD/TOTPは定義を先送り | 未定義のまま全方式をv0.3必須として公開 |
| DR-021 | Envelopeのみ手書き最小パーサーの例外とし、メッセージ本体はスキーマ生成コードに委ねる | 全ワイヤ形式をFlatBuffers等に統一 |
| DR-022 | クリップボード交換にrequest_idを付与し追跡可能にする | request_id無しのFormats/Request/Data |
| DR-023 | リレーの暗号境界をMUST/MAY一覧として明文化 | 「暗号文を中継するだけ」という非規範的な記述のみ |
| DR-024 | Connection SMのActiveは最初の1モニターのVideoStream ChannelがLiveに達した時点で遷移する | 全モニターのStreaming到達を待つ(初期表示が遅くなる) |
| DR-025 | CLIENT_SIDE IMEモード中は、進行中の変換に関わる生KeyEventを送信しない | KeyEventとTextInputを常に併送する(二重入力のリスクを残す) |
| DR-026 | 映像ストリームのgenerationはセッションを通じて単調増加を継続し、再接続時にリセットしない | 再接続のたびにgenerationを0から振り直す |
| DR-027 | VideoStream状態機械をChannel(モニター単位、世代をまたぐ)とInstance(QUICストリーム単位、世代ごと)の2層に分離する | 単一の状態図にChannelレベルとInstanceレベルの遷移を混在させる(v0.3旧版Part 4の問題点) |
| DR-028 | KeepAlive間隔を15秒、IDLE_TIMEOUTをその3倍(45秒)と定義する | 独立に値を決める、またはKeepAliveメッセージ自体を未定義のまま残す |
| DR-029 | 映像バックプレッシャの閾値を絶対値からベースライン相対のdeltaへ再定義する(2.10節の訂正) | 固定の絶対閾値(高遅延経路で伝搬遅延を輻輳と誤検知する欠陥があった) |
| DR-030 | VideoChannelのRecovering失敗時は無期限の指数バックオフ再試行とし(上限30秒でキャップ)、恒久的な「諦め」状態を設けない | 再試行回数に上限を設けてDegraded等の終端状態にエスカレーションする |
| DR-031 | MAX_SESSION_DURATIONは既定24時間・ポリシーで無制限化可能とし、無人セッション向けの運用指針を明記する | 全セッションに一律の強制的な上限を課す |
| DR-032 | Envelope.lengthはpayloadのみのバイト数とし、typeフィールド(2バイト)は含めない | typeを含めた長さとする(v0.1初期案。length<2という無意味なエラー状態を生むだけで撤回) |
| DR-033 | PermissionSetのビット位置を宣言順(VIEW=bit0起点)で正式に割り当てる | ビット位置を実装依存のまま放置する(M2実装での自然発生的な採番を追認) |
| DR-034 | ReasonCode.domain=0を「エラーなし」の予約値とし、4.1/4.6/4.7で名前のみ参照していたコードに正式番号を割り当てる一覧表(4.8.1節)を新設する | 個々の参照箇所に断片的な番号を残したままにする |
| DR-035 | VideoFrameをVideoFrameHeader(CBOR)+VideoFramePayload(生バイト、ラップなし)の連続する2つのEnvelopeに分割する | ヘッダとペイロードを1つのCBOR構造体に混在させる(M3実装がこれで実装し、payload_lenの冗長性とゼロコピー未達成を指摘して確認を求めた) |

---

# Appendix B. Rejected Alternatives / 将来課題

- **VVC等新コーデックの即時対応**: 見送り。`codec` enumと `EncoderConfig`/`DisplayCapabilities` を拡張可能に保つ。
- **USBリダイレクトの全面対応**: 見送り。デバイスクラス許可制+用途プリセットに留める。
- **監査ログの根拠を壁時計に置く**: 却下。単調シーケンス+ハッシュチェーンを根拠とする。
- **AuthMethodをフラットなリストのまま扱う**: 却下。`AuthPolicy.accepted_combinations` + priority に変更。
- **PASSWORD/TOTPの具体プロトコル**: v0.3では定義を先送り。将来版でチャレンジ方式・検証方式(Argon2id等)・試行制限を正式化する。
- **音声のQUIC Datagram化**: 将来課題。v0.3は信頼性ストリーム+クライアント側skip-aheadで簡易に対応する。
- **PermissionSetの3層化(policy/user/effective)**: `granted_permissions` を実効状態と定義することで現行ワイヤのまま将来拡張可能。v0.3では3層構造そのものは導入しない。
