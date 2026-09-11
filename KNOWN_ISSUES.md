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

**付随事項**: 当時は下記「D. バイナリ本体が自動テストの対象外だった」という構造的な問題により、専用の自動回帰テストを追加できていませんでした(Dの修正で`PermissionSm::check_gate`としてテスト可能になっています)。また、現状`server_handshake_from_client_hello`が発行する`granted_permissions`には`FILE_UP`/`FILE_DOWN`が含まれていない(`src/handshake.rs`)ため、実バイナリでファイル転送を試すと常に拒否されます。これは意図した保守的挙動ですが、ファイル転送を実際に使う場合は付与ロジック側の対応が別途必要です。

### C. `FILE_TRANSFER_STALL_TIMEOUT`(仕様4.7、30秒)が未実装だった

`ReasonCode::TRANSPORT_STREAM_STALL_TIMEOUT`は定義済みでしたが、「最終FileChunk受信から30秒」を計測してストリームを強制終了する仕組みはPhase 2c・Task 14のどちらでも実装していませんでした。

**修正**: `file_transfer_session::receive_file_with_timeout`を追加し、チャンク受信ループの各待受を`tokio::time::timeout`でラップしました。タイムアウト時は`FileTransferError{reason: TRANSPORT_STREAM_STALL_TIMEOUT}`をベストエフォートで送信してから`ReceiveOutcome::Error`を返します。既存の`receive_file`は`FILE_TRANSFER_STALL_TIMEOUT`(30秒)を既定値として使う薄いラッパーに変更したため、`sardp-server`側の呼び出し箇所(`spawn_file_transfer`)は変更なしで恩恵を受けます。

`tests/phase2c_file_transfer.rs`に回帰テストを追加(`no_further_chunks_within_the_stall_timeout_yields_stall_timeout_error`、高速化のため50msに差し替えて検証)。

### D. バイナリ本体(`src/bin/*.rs`)が自動テストの対象外だった(旧#1、★最重要)

`cargo test`が通す255件のテストはすべてライブラリクレート(`sardp`)に対するもので、`sardp-server.rs`/`sardp-client.rs`自体のロジックは1行も自動テストの対象になっていませんでした。

**修正**: acceptループの振り分け・`suspend_and_store`/`is_transport_disconnect`・`spawn_file_transfer`本体・ファイル転送とVIEW admin toggleの権限ゲート・`--session-file`読み書きを、それぞれ`src/conn_error.rs`(新規)、`reconnection::establish_connection`、`SessionStore::suspend_and_schedule_expiry`、`file_transfer_session::run_file_transfer`、`PermissionSm::check_gate`/`permission_sm::build_view_toggle`、`src/session_file.rs`(新規)へ切り出しました。バイナリ側は「引数パース→lib呼び出し→ログ/spawn」だけになっています。

lib側にユニットテスト、および実QUIC接続を張った統合テスト(`tests/conn_establish.rs`)を追加。既存255件+新規は`cargo test`のlib 288件+統合39件まで拡大。

### E. リソース枯渇に対する防御がなかった(旧#3)

- `FileHandleStore::issue`で発行したハンドルは、`file`ストリームが一度も開かれなければ`expiry_ts`を過ぎても自動では回収されませんでした。
- `accept_file_stream_verified`の`connection.accept_bi().await`にタイムアウトがありませんでした。
- 同時に受け付けられるファイル転送数の上限もありませんでした。

**修正**: `FileHandleStore::sweep_expired`+`spawn_reaper`(`Weak`参照で自身が生存の唯一の理由にならないよう設計)を追加し、`sardp-server`の`main`で60秒間隔で起動。`accept_file_stream_verified`は`accept_file_stream_verified_with_timeout`(既定30秒、`FileTransferSessionError::AcceptTimeout`)の薄いラッパーに変更(`receive_file`/`receive_file_with_timeout`と同じパターン)。`FileHandleStore::issue`は内部的に`try_issue`(容量チェック付き)を呼ぶ形にし、`sardp-server`は`MAX_CONCURRENT_FILE_TRANSFERS`(64)の上限に達すると`FileTransferReject{reason: POLICY_FILE_POLICY_REJECTED}`で拒否するようにしました(spec 4.8.1のReasonCode表に専用コードがないため、既存コードのうち最も意味が近いものを流用)。

### F. handshake.rsの分割でHANDSHAKE_TIMEOUTが最大2倍に伸びていた(旧#6)

acceptループの振り分けを可能にするため`server_handshake_with_timeouts`をClientHello読み取り部分と`server_handshake_from_client_hello`に分割した結果、「ClientHello待ち」と「ServerHello送信」がそれぞれ独立に`handshake_timeout`(既定10秒)でラップされ、理論上の最悪ケースが最大20秒まで伸びていました。

**修正**: `read_first_control_message`/`server_handshake_from_client_hello`の引数を`handshake_timeout: Duration`から共有の`handshake_deadline: tokio::time::Instant`に変更し、`tokio::time::timeout`ではなく`timeout_at`でそのデッドラインを共有する形にしました。デッドラインは`establish_connection`(acceptループ経由)・`server_handshake_with_timeouts`(モノリシック経路、テストが使う`server_handshake`もこちら)それぞれの入口で1回だけ計算し、両フェーズに使い回します。

`tests/handshake_timeout_budget.rs`に回帰テストを追加。`read_first_control_message`は実際にネットワーク入力を待つ(=本物のブロッキングポイントがある)ため、既に経過したデッドラインを渡すと即座に`HandshakeTimeout`になることを直接検証できます。一方`server_handshake_from_client_hello`側の`ServerHello`送信は、QUICストリームの初期フロー制御ウィンドウ内に収まる小さな書き込みが実際にはブロックしない(=`tokio::time::timeout_at`が経過済みデッドラインを観測する機会がない)ため、同じ手法でのランタイム上の実証はできません。こちらの修正は「`Duration`を受け取って独自の新しいウィンドウを再計算する」という選択肢自体をシグネチャから排除したことで担保しています。

### G. クリップボード・音声が実バイナリに未配線だった(旧#12)

`clipboard_session.rs`・`audio_session.rs`はライブラリ層+実QUIC結合テストのみで検証済みで、`sardp-server`/`sardp-client`本体には配線していませんでした。

**修正**: 以下の3方向を実配線しました(1-Aで確立したパターンに従い、新規ロジックはlib側に切り出してユニット/統合テストを追加)。

- **`AUDIO_PLAYBACK`(server→client)**: `sardp-server`が`AUDIO_PLAYBACK`許可時に`audio_playback`ストリームを遅延オープンし、20ms間隔で合成サイン波フレームを送信(`audio_session::generate_sine_wave_payload`、新規)。VIEW/frame送出ループと同じく`permission_sm`のライブ状態で毎回ゲート。`sardp-client`は`accept_uni()`で常時待受し、受信フレームをログ出力。
- **`AUDIO_CAPTURE`(client→server)**: `sardp-client`はハンドシェイク時点の`granted_permissions`に`AUDIO_CAPTURE`が含まれていれば`audio_capture`ストリームを起動時に一度だけ開き、20ms間隔で合成無音フレーム(`audio_session::generate_silence_payload`、新規)を送信。`sardp-server`は新設の`audio_session::accept_audio_capture_gated`(DR-037のownership rejectと同じ「拒否するなら能動的に`stop()`する」形)でアクセプト時に許可を確認し、拒否時はストリームを`stop()`して破棄、許可時は専用タスクにフレーム読み取りを委譲(`spawn_file_transfer`と同じ独立タスクパターン)。
- **`CLIP_READ`(server→client、announcer=server)**: `sardp-server`の新しい管理者stdinコマンド`send-clipboard`で、`CLIP_READ`が許可されていれば固定の合成`text/plain`コンテンツを1回announceするタスクを起動(`announce_clipboard_formats`→`read_clipboard_request`→`respond_to_clipboard_request`、いずれも既存の1-A前からある関数)。`sardp-client`は`accept_bi()`で常時待受し、announceを受けたら自動で最初のフォーマットを`request_clipboard_data`でリクエストしてログ出力。

VIEWのみだった管理者stdinトグル機構は`permission_sm::AdminCommand`/`parse_admin_command`(新規、ユニットテスト付き)として汎化し、`grant-clip-read`/`revoke-clip-read`・`grant-clip-write`/`revoke-clip-write`・`grant-audio-playback`/`revoke-audio-playback`・`grant-audio-capture`/`revoke-audio-capture`・`send-clipboard`を追加。`permission_sm::build_view_toggle`も任意ビットに使える`build_permission_toggle(current_granted, bit, grant)`に一般化しました。

`ConnError`/`AppError`にそれぞれ`Audio(AudioError)`variantを追加し、`is_transport_disconnect`にも`AudioError::Quic`系の分類を追加(サーバー側の`accept_audio_capture_gated`アーム・クライアント側の`accept_audio_stream`アームが、接続断のときに正しく`?`で伝播してセッション終了・再接続フローに乗るようにするため — さもないと「常にすぐ失敗する`accept`をタイトループで呼び続ける」というビジーループになり得ます)。

**未対応の付随事項**:
1. **`CLIP_WRITE`(client→server、announcer=client)は実配線していません。** サーバー側で`accept_bi()`を追加すると、`spawn_file_transfer`が個別タスクで独立に呼んでいる`accept_bi()`と同じ着信bidiストリームのキューを取り合うことになり、どちらのタスクが実際にどのストリームを受け取るか非決定的になる(ファイル転送用に開かれたはずのストリームをクリップボード側が奪う、またはその逆)というレースが生じます。単一の集中ディスパッチャに一本化しない限り安全に追加できないため、今回はスコープ外としました。`grant-clip-write`/`revoke-clip-write`コマンド自体は存在しますが、対応する消費者はまだありません。
2. `AUDIO_CAPTURE`は`sardp-client`起動時の`granted_permissions`スナップショットでのみ判定しており、セッション中に後から`PermissionUpdate`で許可されても反応しません(サーバー側に対話的な管理者トグルがあるのに対し、クライアント側にはstdinのような対話手段がないための割り切りです)。`AUDIO_PLAYBACK`/`CLIP_READ`はサーバー側が毎回ライブに`permission_sm`を見るため、後から`grant-*`しても効きます。
3. 実バイナリ同士でこの配線をエンドツーエンドに手動確認することはできていません。ビデオchannelを開く(`open_generation`)処理がハンドシェイク直後に必ず走り、その中の`ffmpeg`呼び出しがこのサンドボックスには存在しないため、`run_active_session`(clipboard/audioの配線はすべてこの中)に到達する前にセッションが終了してしまいます。ライブラリレベルのユニット/統合テスト(`accept_audio_capture_gated`・`parse_admin_command`・`is_transport_disconnect`のAudio分類など)でのみ検証済みです。

## 未対応(現在のスコープでは許容している既知の課題)

### 2. suspend-on-disconnectの検証パターンが手動テスト1回分に限られている

`is_transport_disconnect`(`src/bin/sardp-server.rs`)は、video送出パス経由の切断を手動でkill -9して初めて漏れ(`ConnError::Video`が未分解だった)に気づいて直したという経緯があり、それ以外の経路(controlストリーム読み取り中、feedback読み取り中、backpressure再オープン中の切断)は実際に切ってみて確認していません。コード上は同じパターンで拾えるはずですが未検証です。

### 4. `--session-file`はデモ専用の割り切りで、平文で資格情報相当を保存する

`session_id`/`reconnect_token`/`user_id`を任意パスに平文・パーミッション制御なしで書き出す実装です(`src/bin/sardp-client.rs`)。`reconnect_token`はbearer credential(所持のみで再接続が成立する)なので、このファイルの中身が漏れることは元のセッションを乗っ取られることと同義です。実バイナリ間での再接続の往復を証明する唯一の現実的な手段として追加しましたが、デモ・テスト以外の用途に転用すべきではありません。

### 5. クライアント側に自動再接続ロジックがない(サーバーとの非対称性)

サーバーは接続断を検知して自動的にSuspendedへ遷移しますが、クライアント側には同等の検知・再接続ロジックが一切ありません。プロセスが生きたまま接続だけ切れた場合、クライアントは単にエラーで終了します。今回実証した「再接続の往復」は、プロセスを手動で再起動して`--session-file`を読ませる形でのみ成立しており、実運用で想定される「プロセスは生きたままネットワークだけ復旧する」ケースはカバーしていません。

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
