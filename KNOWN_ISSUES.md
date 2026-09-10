# 既知の課題・技術的負債

PoCブリーフのPhase 1〜3 + Task 14(spencomeister/SARDP#1)の実装を通じて、実装者自身が把握している技術的負債・設計上の妥協点をまとめたものです。会話ログの中だけに残すよりリポジトリ側に残す方が後で参照しやすいという判断で作成しました。

各項目は「対応済み」「未対応」で分けています。未対応の項目はこのPoCの現在のスコープでは許容していますが、Phase 4以降(実OS統合・TCPフォールバック・実認証・プロセス分離・デプロイ・外部監査)に進む、またはこのコードを実運用に近い形で使う場合は、着手前に見直すべき項目です。

## 対応済み(spencomeister/SARDP#1へのレビューコメントで指摘、修正済み)

### A. `SessionStore`のexpireタイマーの競合

`suspend_and_store`(`src/bin/sardp-server.rs`)が`RECONNECT_GRACE_PERIOD`(300秒)後に`SessionStore`からエントリを削除するバックグラウンドタスクを積んでいましたが、当初は`SessionStore::expire(session_id)`という「session_idに対して現在何が入っていようと無条件に消す」メソッドを呼んでいました。

suspend→reconnect→再suspendという流れが起きると、最初のsuspend用に積んだ古いタイマーが、2回目のsuspendによってまだ有効な猶予期間中にあるエントリを誤って削除してしまう競合がありました。

**修正**: `SessionStore::expire_if_token_matches(session_id, reconnect_token)`を追加(`src/session_store.rs`)。`suspend()`のたびに新しい`reconnect_token`が発行されることを利用し、「そのタイマーが積まれた時点のsuspendエピソードが今もストアに残っているか」をトークンの一致で判定してから削除するようにしました。`expire`自体は(明示的な強制削除が必要な場合のために)無条件版として残しています。

回帰テストを`src/session_store.rs`に3件追加(`expire_if_token_matches_removes_the_matching_episode`、`expire_if_token_matches_is_a_no_op_for_a_stale_token`、`a_stale_expiry_timer_does_not_clobber_a_later_suspend_episode`)。最後のテストが実際の競合シナリオ(suspend→reconnect→再suspend→古いタイマー発火)を再現し、新しいエントリが生き残ることを確認しています。

### B. ファイル転送に`FILE_UP`/`FILE_DOWN`権限ゲートがなかった

`permission_set::bit::FILE_UP`/`FILE_DOWN`は定義済みでしたが、`sardp-server`の`FileTransferRequest`処理はこれらを一切チェックせず、権限の有無に関わらず`file_handle`を発行していました。VIEW権限には同種のゲートがあった(`permission_sm.is_granted(bit::VIEW)`)のに、ファイル転送では見落としていました。

**修正**: `FileTransferRequest`受信時に、`direction`に応じて`FILE_UP`/`FILE_DOWN`が`Granted`かを確認し、`Granted`でなければ`FileTransferReject`で応答するようにしました(`src/bin/sardp-server.rs`)。`PermissionSm::state()`を使って`Draining`(段階的revoke中)と`NotGranted`を区別し、前者は`POLICY_PERMISSION_REVOKED`、後者は`POLICY_PERMISSION_DENIED`を返します(仕様4.5の表の区別に合わせています)。

**未対応の付随事項**: この修正は`sardp-server.rs`というバイナリ内のロジックであり、下記「1. バイナリ本体が自動テストの対象外」という構造的な問題により、専用の自動回帰テストは追加していません。コードレビューでの確認のみです。また、現状`server_handshake_from_client_hello`が発行する`granted_permissions`には`FILE_UP`/`FILE_DOWN`が含まれていない(`src/handshake.rs`)ため、実バイナリでファイル転送を試すと常に拒否されます。これは意図した保守的挙動ですが、ファイル転送を実際に使う場合は付与ロジック側の対応が別途必要です。

### C. `FILE_TRANSFER_STALL_TIMEOUT`(仕様4.7、30秒)が未実装だった

`ReasonCode::TRANSPORT_STREAM_STALL_TIMEOUT`は定義済みでしたが、「最終FileChunk受信から30秒」を計測してストリームを強制終了する仕組みはPhase 2c・Task 14のどちらでも実装していませんでした。

**修正**: `file_transfer_session::receive_file_with_timeout`を追加し、チャンク受信ループの各待受を`tokio::time::timeout`でラップしました。タイムアウト時は`FileTransferError{reason: TRANSPORT_STREAM_STALL_TIMEOUT}`をベストエフォートで送信してから`ReceiveOutcome::Error`を返します。既存の`receive_file`は`FILE_TRANSFER_STALL_TIMEOUT`(30秒)を既定値として使う薄いラッパーに変更したため、`sardp-server`側の呼び出し箇所(`spawn_file_transfer`)は変更なしで恩恵を受けます。

`tests/phase2c_file_transfer.rs`に回帰テストを追加(`no_further_chunks_within_the_stall_timeout_yields_stall_timeout_error`、高速化のため50msに差し替えて検証)。

## 未対応(現在のスコープでは許容している既知の課題)

### 1. バイナリ本体(`src/bin/*.rs`)が自動テストの対象外という構造的な穴 ★最重要

`cargo test`が通す255件のテストはすべてライブラリクレート(`sardp`)に対するもので、`sardp-server.rs`/`sardp-client.rs`自体のロジックは1行も自動テストの対象になっていません。acceptループの振り分け、`suspend_and_store`/`is_transport_disconnect`、`spawn_file_transfer`、`--session-file`の読み書き、そして今回追加した権限ゲートも含め、すべて手動検証(または今回のようなコードレビュー)でしか確認していません。テストが通っていても、これらの配線がリグレッションしても`cargo test`は気づけません。

対処するなら、ロジックをライブラリ側の関数に切り出してテスト可能にするか、実バイナリをsubprocessとして起動する統合テストを別途用意する必要があります。

### 2. suspend-on-disconnectの検証パターンが手動テスト1回分に限られている

`is_transport_disconnect`(`src/bin/sardp-server.rs`)は、video送出パス経由の切断を手動でkill -9して初めて漏れ(`ConnError::Video`が未分解だった)に気づいて直したという経緯があり、それ以外の経路(controlストリーム読み取り中、feedback読み取り中、backpressure再オープン中の切断)は実際に切ってみて確認していません。コード上は同じパターンで拾えるはずですが未検証です。

### 3. リソース枯渇に対する防御がない

- `FileHandleStore::issue`で発行したハンドルは、`file`ストリームが一度も開かれなければ`expiry_ts`を過ぎても自動では回収されません(`validate`は参照時に期限切れを検出するだけで、能動的な掃除タスクがない)。
- `spawn_file_transfer`内の`accept_file_stream_verified`は`connection.accept_bi().await`にタイムアウトを掛けていません。`FileTransferRequest`だけ送って`file`ストリームを開かないクライアントがいれば、ハンドルもタスクも溜まり続けます。
- 同時に受け付けられるファイル転送数の上限もありません。

### 4. `--session-file`はデモ専用の割り切りで、平文で資格情報相当を保存する

`session_id`/`reconnect_token`/`user_id`を任意パスに平文・パーミッション制御なしで書き出す実装です(`src/bin/sardp-client.rs`)。`reconnect_token`はbearer credential(所持のみで再接続が成立する)なので、このファイルの中身が漏れることは元のセッションを乗っ取られることと同義です。実バイナリ間での再接続の往復を証明する唯一の現実的な手段として追加しましたが、デモ・テスト以外の用途に転用すべきではありません。

### 5. クライアント側に自動再接続ロジックがない(サーバーとの非対称性)

サーバーは接続断を検知して自動的にSuspendedへ遷移しますが、クライアント側には同等の検知・再接続ロジックが一切ありません。プロセスが生きたまま接続だけ切れた場合、クライアントは単にエラーで終了します。今回実証した「再接続の往復」は、プロセスを手動で再起動して`--session-file`を読ませる形でのみ成立しており、実運用で想定される「プロセスは生きたままネットワークだけ復旧する」ケースはカバーしていません。

### 6. handshake.rsの分割でHANDSHAKE_TIMEOUTの意味が変わった

acceptループの振り分けを可能にするため`server_handshake_with_timeouts`をClientHello読み取り部分と`server_handshake_from_client_hello`に分割した結果、「ClientHello待ち」と「ServerHello送信」がそれぞれ独立に`handshake_timeout`(既定10秒)でラップされる形になりました。以前は1つの10秒予算を共有していたのに対し、理論上の最悪ケースが最大20秒まで伸びています。専用のテストは書いておらず、仕様の「HANDSHAKE_TIMEOUT = 10秒」を厳密な合計上限と読むなら軽微な逸脱です。

### 7. `file_handle`を仕様の`bytes(16)`ではなく`u64`として実装

`StreamPrologue.context_id`がvarint(上限2^62-1)であり16バイトを直接運べないための簡略化です。コード内コメント(`src/messages.rs`のdoc comment)で明記済みです。

### 8. Phase 2aの`ChannelState::Paused`とバックプレッシャの相互作用

Pausedな状態で`on_reset()`/`on_instance_streaming()`が無条件で作動し、Pausedだったという情報が復旧後に失われます(仕様の状態遷移図自体がこのケースを明記していないため、意図的な簡略化として扱っています)。

### 9. `SessionStore`/`FileHandleStore`はプロセス内メモリのみ

単一プロセスのサーバーを前提としており、サーバー再起動やPhase 7以降のプロセス分離・複数インスタンス構成では前提から崩れます。

### 10. 音声はメッセージの配管のみで、補正ロジックは何も実装していない

`AudioSyncFeedback`はワイヤ形式と読み書きのみを実装しており、実際のクロックドリフト補正(再生レート微調整・ジッターバッファ・skip-ahead)は一切実装していません。仕様がSHOULD/MAYレベルで「アルゴリズムは規定しない」としているため未実装であること自体は仕様に反しませんが、「クロックドリフト補正」という言葉から実際の補正動作を期待すると誤解を招くため明記しておきます。

### 11. `FeedbackReceiver`の`read_one`/`read_message`の重複

`read_one`(`TransportFeedback`専用)と`read_message`(両方対応)がほぼ重複した形で共存しています(`src/feedback_session.rs`)。将来どちらかだけ修正されて挙動がずれるリスクがあります。

### 12. クリップボード・音声は実バイナリに未配線

`clipboard_session.rs`・`audio_session.rs`はライブラリ層+実QUIC結合テストでのみ検証済みで、`sardp-server`/`sardp-client`本体には配線していません。Phase 2a〜2c・Phase 3全体を通じて一貫してこのスコープで進めています。
