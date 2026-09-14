# 既知の課題・技術的負債

PoCブリーフのPhase 1〜3 + Task 14(spencomeister/SARDP#1)の実装を通じて、実装者自身が把握している技術的負債・設計上の妥協点をまとめたものです。会話ログの中だけに残すよりリポジトリ側に残す方が後で参照しやすいという判断で作成しました。

各項目は「対応済み」「未対応」で分けています。未対応の項目はこのPoCの現在のスコープでは許容していますが、Phase 4以降(実OS統合・TCPフォールバック・実認証・プロセス分離・デプロイ・外部監査)に進む、またはこのコードを実運用に近い形で使う場合は、着手前に見直すべき項目です。

## 対応済み(spencomeister/SARDP#1へのレビューコメントで指摘、修正済み)

### A. `SessionStore`のexpireタイマーの競合

`suspend_and_store`(`sardp-cli/src/bin/sardp-server.rs`)が`RECONNECT_GRACE_PERIOD`(300秒)後に`SessionStore`からエントリを削除するバックグラウンドタスクを積んでいましたが、当初は`SessionStore::expire(session_id)`という「session_idに対して現在何が入っていようと無条件に消す」メソッドを呼んでいました。

suspend→reconnect→再suspendという流れが起きると、最初のsuspend用に積んだ古いタイマーが、2回目のsuspendによってまだ有効な猶予期間中にあるエントリを誤って削除してしまう競合がありました。

**修正**: `SessionStore::expire_if_token_matches(session_id, reconnect_token)`を追加(`src/session_store.rs`)。`suspend()`のたびに新しい`reconnect_token`が発行されることを利用し、「そのタイマーが積まれた時点のsuspendエピソードが今もストアに残っているか」をトークンの一致で判定してから削除するようにしました。`expire`自体は(明示的な強制削除が必要な場合のために)無条件版として残しています。

回帰テストを`src/session_store.rs`に3件追加(`expire_if_token_matches_removes_the_matching_episode`、`expire_if_token_matches_is_a_no_op_for_a_stale_token`、`a_stale_expiry_timer_does_not_clobber_a_later_suspend_episode`)。最後のテストが実際の競合シナリオ(suspend→reconnect→再suspend→古いタイマー発火)を再現し、新しいエントリが生き残ることを確認しています。

### B. ファイル転送に`FILE_UP`/`FILE_DOWN`権限ゲートがなかった

`permission_set::bit::FILE_UP`/`FILE_DOWN`は定義済みでしたが、`sardp-server`の`FileTransferRequest`処理はこれらを一切チェックせず、権限の有無に関わらず`file_handle`を発行していました。VIEW権限には同種のゲートがあった(`permission_sm.is_granted(bit::VIEW)`)のに、ファイル転送では見落としていました。

**修正**: `FileTransferRequest`受信時に、`direction`に応じて`FILE_UP`/`FILE_DOWN`が`Granted`かを確認し、`Granted`でなければ`FileTransferReject`で応答するようにしました(`sardp-cli/src/bin/sardp-server.rs`)。`PermissionSm::state()`を使って`Draining`(段階的revoke中)と`NotGranted`を区別し、前者は`POLICY_PERMISSION_REVOKED`、後者は`POLICY_PERMISSION_DENIED`を返します(仕様4.5の表の区別に合わせています)。

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

### G. fileストリームが双方向実装のままで、CLIP_WRITE追加時にaccept_bi()が競合する構造だった(DR-038)

Phase 2c以来、`file`ストリームは`open_bi`/`accept_bi`で実装されており、`FileTransferComplete`送信後の`FileTransferError`逆方向通知を同じ双方向ストリームに乗せていました。規範仕様2.6節は元々「`file`はMUSTで単方向」としていましたが、実装はこれに反していました。下記Hの`CLIP_WRITE`配線時、サーバー側で`clipboard`用の`accept_bi()`を追加しようとすると、`spawn_file_transfer`が個別タスクで独立に呼んでいた`accept_bi()`と同じ着信bidiストリームのキューを取り合うことになり、どちらのタスクが実際にどの着信ストリームを受け取るか非決定的になる(ファイル転送用に開かれたはずのストリームをクリップボード側が奪う、またはその逆)という競合が生じることが判明しました。

**修正**: `file`ストリームを規範仕様どおり単方向(`open_uni`/`accept_uni`)に戻しました(DR-038、規範仕様2.6節に明記)。

- `open_file_stream`/`accept_file_stream`/`accept_file_stream_verified[_with_timeout]`を`open_uni`/`accept_uni`ベースに変更。所有権検証失敗時の切断も、双方向前提の`send.reset()`から単方向の`reader.stop()`(`EnvelopeReader`に新規追加)に変更。
- `FileTransferError`は`file`ストリームではなく、既に双方向で開いている`control`ストリーム上で`file_handle`により相関付けて送るように変更(規範仕様2.6節)。これに伴い、`receive_file`/`receive_file_with_timeout`は一切の書き込みを行わなくなり、`FileTransferError`を検出結果として返すだけの純粋な受信ロジックになりました(1-Aのテスト可能パターンにさらに沿う形)。実際に`control`へ書き込むのは、そのストリームへのアクセス権を持つ呼び出し側(`sardp-server`の`spawn_file_transfer`)の責務です。
- **副次的に見つかった潜在バグの修正**: `direction`がDownload(サーバーが送信側)の場合でも、旧実装はサーバー側が常に`accept_bi()`していました(双方向ストリームなら、どちらが開いてもどちらの半分でも送受信できるため表面化しなかった潜在的な設計不整合)。単方向化するとこれは成立しなくなる(単方向ストリームは開いた側しか書き込めない)ため、`run_file_transfer`をdirectionに応じて分岐させ、Uploadはサーバーがaccept、Downloadはサーバーがopenするように修正しました(`tests/phase2c_file_transfer.rs`に回帰テストを追加、旧実装ではDownload方向のテストが一件も存在しませんでした)。
- `run_file_transfer`の戻り値を`FileTransferOutcome::{Done, ReportError(FileTransferError)}`に変更し、呼び出し側(`sardp-server`)が`ReportError`を受け取ったら自身で`control`ストリームに書き込む形にしました。この書き込みは`run_active_session`のメインループが持つ`control`の送信半分と競合しうるため(`spawn_file_transfer`は独立タスク)、`Arc<tokio::sync::Mutex<quinn::SendStream>>`(`SharedControlSend`)で共有し、書き込みのたびに短時間ロックする方式にしました。

`clipboard`との`accept_bi()`競合は、`file`が`accept_bi()`を一切使わなくなったことで構造的に解消されています(現状、サーバー側で`accept_bi()`を呼ぶのは下記Hで新設した`clipboard`受理アームのみ)。ただし、これは「`accept_bi()`同士の競合」を解消しただけで、「`accept_uni()`同士の競合」までは解消していない点に注意してください。`sardp-server`は`accept_uni()`の呼び出し元を複数持ちます(`FeedbackReceiver::accept`は一度きりで`run_active_session`開始前に完了するため無害ですが、`accept_audio_capture_gated`の常時待受アームと、Upload方向の`file`ストリームを受理する`spawn_file_transfer`の個別タスクは、両方とも`run_active_session`の実行中いつでも動き得ます)。理論上は同一クライアントが`AUDIO_CAPTURE`許可下でオーディオ配信中に、同時にファイルをアップロードした場合、この2つが着信uniストリームのキューを取り合う可能性があります。現状の実バイナリでは(a) `AUDIO_CAPTURE`がデフォルトで許可されない、(b) 対話的な`sardp-client`はそもそもファイル転送を自発的に開始しない、という理由でこの経路は到達しませんが、構造的な限界として記録しておきます。

### H. クリップボード・音声が実バイナリに未配線だった(旧#12)

`clipboard_session.rs`・`audio_session.rs`はライブラリ層+実QUIC結合テストのみで検証済みで、`sardp-server`/`sardp-client`本体には配線していませんでした。

**修正**: 以下の4方向を実配線しました(1-Aで確立したパターンに従い、新規ロジックはlib側に切り出してユニット/統合テストを追加)。

- **`AUDIO_PLAYBACK`(server→client)**: `sardp-server`が`AUDIO_PLAYBACK`許可時に`audio_playback`ストリームを遅延オープンし、20ms間隔で合成サイン波フレームを送信(`audio_session::generate_sine_wave_payload`、新規)。VIEW/frame送出ループと同じく`permission_sm`のライブ状態で毎回ゲート。`sardp-client`は`accept_uni()`で常時待受し、受信フレームをログ出力。
- **`AUDIO_CAPTURE`(client→server)**: `sardp-client`はハンドシェイク時点の`granted_permissions`に`AUDIO_CAPTURE`が含まれていれば`audio_capture`ストリームを起動時に一度だけ開き、20ms間隔で合成無音フレーム(`audio_session::generate_silence_payload`、新規)を送信。`sardp-server`は新設の`audio_session::accept_audio_capture_gated`(DR-037のownership rejectと同じ「拒否するなら能動的に`stop()`する」形)でアクセプト時に許可を確認し、拒否時はストリームを`stop()`して破棄、許可時は専用タスクにフレーム読み取りを委譲(`spawn_file_transfer`と同じ独立タスクパターン)。
- **`CLIP_READ`(server→client、announcer=server)**: `sardp-server`の新しい管理者stdinコマンド`send-clipboard`で、`CLIP_READ`が許可されていれば固定の合成`text/plain`コンテンツを1回announceするタスクを起動(`announce_clipboard_formats`→`read_clipboard_request`→`respond_to_clipboard_request`、いずれも既存の1-A前からある関数)。`sardp-client`は`accept_bi()`で常時待受し、announceを受けたら自動で最初のフォーマットを`request_clipboard_data`でリクエストしてログ出力。
- **`CLIP_WRITE`(client→server、announcer=client)**: 上記Gの修正で`file`ストリームが`accept_bi()`を使わなくなったため、`sardp-server`の`run_active_session`に`accept_clipboard_formats`の常時待受アームを追加できるようになりました。`CLIP_WRITE`が許可されていれば自動で最初のフォーマットをリクエスト、許可されていなければ何もリクエストしない(spec 2.7上、announceに対して何もリクエストしないこと自体が正当な応答であり、専用の拒否メッセージは不要)。`sardp-client`側もハンドシェイク時点で`CLIP_WRITE`が許可されていれば起動時に1回announceする(`clipboard_announce_once`、`AUDIO_CAPTURE`と同じ「起動時スナップショットのみ判定」という割り切り)。

VIEWのみだった管理者stdinトグル機構は`permission_sm::AdminCommand`/`parse_admin_command`(新規、ユニットテスト付き)として汎化し、`grant-clip-read`/`revoke-clip-read`・`grant-clip-write`/`revoke-clip-write`・`grant-audio-playback`/`revoke-audio-playback`・`grant-audio-capture`/`revoke-audio-capture`・`send-clipboard`を追加。`permission_sm::build_view_toggle`も任意ビットに使える`build_permission_toggle(current_granted, bit, grant)`に一般化しました。

`ConnError`/`AppError`にそれぞれ`Audio(AudioError)`/`Clipboard(ClipboardSessionError)`variantを追加し、`is_transport_disconnect`にも両方の`Quic`系分類を追加(サーバー側の`accept_audio_capture_gated`/`accept_clipboard_formats`アームが、接続断のときに正しく`?`で伝播してセッション終了・再接続フローに乗るようにするため — さもないと「常にすぐ失敗する`accept`をタイトループで呼び続ける」というビジーループになり得ます)。

**未対応の付随事項**:
1. `AUDIO_CAPTURE`/`CLIP_WRITE`は`sardp-client`起動時の`granted_permissions`スナップショットでのみ判定しており、セッション中に後から`PermissionUpdate`で許可されても反応しません(サーバー側に対話的な管理者トグルがあるのに対し、クライアント側にはstdinのような対話手段がないための割り切りです)。`AUDIO_PLAYBACK`/`CLIP_READ`/`CLIP_WRITE`(サーバー側の受理判定)はサーバー側が毎回ライブに`permission_sm`を見るため、後から`grant-*`しても効きます。
2. 実バイナリ同士でこの配線をエンドツーエンドに手動確認することはできていません。ビデオchannelを開く(`open_generation`)処理がハンドシェイク直後に必ず走り、その中の`ffmpeg`呼び出しがこのサンドボックスには存在しないため、`run_active_session`(clipboard/audioの配線はすべてこの中)に到達する前にセッションが終了してしまいます。ライブラリレベルのユニット/統合テスト(`accept_audio_capture_gated`・`parse_admin_command`・`is_transport_disconnect`のAudio/Clipboard分類など)でのみ検証済みです。

### I. `tests/m6_measurement.rs`の`netem_available()`呼び出しが`NETEM_LOCK`の保護範囲外だった

`wan_profile_transport_latency_is_within_150ms`/`high_rtt_via_real_netem_never_enters_congested`はいずれも`netem::netem_available()`(ルールを試験的に`add`→`del`して可用性を判定するprobe)を`NETEM_LOCK.lock().await`の**前**に呼んでいました。実際のプロファイル適用(`apply_profile`、`tc qdisc replace`)は`NETEM_LOCK`の保護下でしか行われないのに対し、この可用性probeだけはロック外で実行されていたため、`cargo test --test m6_measurement`のデフォルト並列実行時に、あるテストの`netem_available()`のprobe(`add`)が、別のテストが同時に`NETEM_LOCK`保持下で適用中のプロファイル(`replace`で既にroot qdiscが存在する状態)と衝突し、`tc`が失敗して`netem_available()`が実際には使える環境でも`false`を返す(結果としてWANテストが誤ってスキップされる)ことがありました。EC2実機での検証で、並列実行時に`wan_profile_transport_latency_is_within_150ms`が誤スキップされる一方、`--test-threads=1`の直列実行では3テストとも実netemで正しく計測できる(LAN transport_us=646/691、WAN transport_us=80689)ことを確認して発見されました。

**修正**: 両テストで`NETEM_LOCK.lock().await`を`netem_available()`呼び出しより前に移動し、可用性probeから(適用されていれば)`NetemGuard`のdropによるclearまでを、ひとつの連続したロック区間に収めました。これにより`netem_available()`のprobe自体が実質的に`NETEM_LOCK`の保護下で実行されることになり、他テストの`tc`操作との競合が構造的になくなります(`src/netem.rs`自体は変更なし — ロックはテストバイナリ側の`static`のため、呼び出し順序の修正で対応)。

## 未対応(現在のスコープでは許容している既知の課題)

### 2. suspend-on-disconnectの検証パターンが手動テスト1回分に限られている

`is_transport_disconnect`(`sardp-cli/src/bin/sardp-server.rs`)は、video送出パス経由の切断を手動でkill -9して初めて漏れ(`ConnError::Video`が未分解だった)に気づいて直したという経緯があり、それ以外の経路(controlストリーム読み取り中、feedback読み取り中、backpressure再オープン中の切断)は実際に切ってみて確認していません。コード上は同じパターンで拾えるはずですが未検証です。

### 4. `--session-file`はデモ専用の割り切りで、平文で資格情報相当を保存する

`session_id`/`reconnect_token`/`user_id`を任意パスに平文・パーミッション制御なしで書き出す実装です(`sardp-cli/src/bin/sardp-client.rs`)。`reconnect_token`はbearer credential(所持のみで再接続が成立する)なので、このファイルの中身が漏れることは元のセッションを乗っ取られることと同義です。実バイナリ間での再接続の往復を証明する唯一の現実的な手段として追加しましたが、デモ・テスト以外の用途に転用すべきではありません。

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

## Stage 3(Windows OS統合、`tools/dxgi-capture-poc`)関連

Stage 3ロードマップの3W-1(在席キャプチャ・エンコード基盤)実装中に判明した、SARDP本体ではなく検証機材・OS依存の既知事項です。3W-1-d(SARDP本体への結線)や他OSでの展開時に見直すべき点として記録します。

### 12. H.264ハードウェアエンコードはMedia Foundation経由で、この検証機ではNVIDIA機でのみ動作確認済み

3W-1-bは`IMFTransform`を直接駆動する構成でハードウェアH.264エンコードを実装しました(`tools/dxgi-capture-poc/src/bin/mf_h264_encode.rs`)。この検証機ではNVIDIA GPU上で「NVIDIA H.264 Encoder MFT」(D3D11対応・非同期)を使っており、実質的にNVENCがMedia Foundation経由で動いている状態です。

- **ベンダー差はMFの抽象化レイヤーが吸収する設計**: `find_hardware_h264_encoder`はベンダー名を一切ハードコードしておらず、`MFT_ENUM_FLAG_HARDWARE`でハードウェアカテゴリのMFTを列挙して先頭を使うだけです。理論上、同じコードがIntel Quick SyncやAMD AMFのMFTでも動くはずです。
- **ただしIntel/AMD環境では未検証**: 検証機がNVIDIA GPU搭載機のみのため、実際にIntel Quick SyncやAMD AMFのMFT経由で動作するかは確認できていません。特に3W-1-bで踏んだ「入力フォーマットがNV12のみ(BGRA不可)」「非同期MFTは`MF_TRANSFORM_ASYNC_UNLOCK`必須」といった制約がベンダーMFTごとにどう変わるかは未知数です。
- **ハードウェアエンコーダが存在しない環境向けのTier 4(ソフトウェアエンコード)フォールバックが未実装**: `mf_probe`(`tools/dxgi-capture-poc/src/bin/mf_probe.rs`)はこの検証機で、NVIDIAのMFTに加えて「H264 Encoder MFT」(MicrosoftのソフトウェアエンコーダでMF_SA_D3D11_AWARE=0)も検出しています。Part 6のTier区分でいうTier 4(ソフトウェアエンコード)フォールバックの実装候補になり得ますが、現在の`find_hardware_h264_encoder`は`MFT_ENUM_FLAG_HARDWARE`のみを見るため、ハードウェアエンコーダが1つも見つからない環境では単純にエラーになります。同じ`IMFTransform`駆動ロジックのまま、フォールバック時に列挙フラグからHARDWAREを外して選択するMFTを切り替えるだけで対応できる見込みです。

### 13. `SendInput`によるTextInput注入の安全な文字間隔は、相手のコントロール実装に依存し一般化できない

3W-1-c(`tools/dxgi-capture-poc/src/bin/send_input_poc.rs`)は、`KEYEVENTF_UNICODE`で文字列を1文字ずつ`SendInput`する際、文字間に50msの待機を挟んでいます。これは実測で必要だった値です(文字列全体を1回の`SendInput`にまとめて送ると文字の脱落・重複が発生し、15ms間隔でもまだ発生しました)。

`send_input_timing_probe`(`tools/dxgi-capture-poc/src/bin/send_input_timing_probe.rs`)で、この待機が「Windowsの入力キュー全般に必要な間隔」なのか「検証対象固有の問題」なのかを切り分けました。結果:

- 自前で作った古典的なWin32 EDITコントロール(システム標準の"EDIT"ウィンドウクラス)へは、**待機0msでも文字化けしない**(全5パターン: 0/1/5/15/50msいずれもPASS)。
- 一方、検証に使ったモダン化メモ帳(WinUIベースと見られるテキストコントロール)では15msでもまだ文字化けし、50msで初めて安定した。

つまり安全な文字間隔は相手のコントロール実装(古典的なWin32コントロールか、モダンなXAML/WinUI系コントロールか)に強く依存し、単一の定数では一般化できないことが分かりました。SARDP本体(規範仕様2.12節、TextInput)で任意長の文章を打ち込む実装をする際、リモート側でどのアプリ・どのコントロールにフォーカスがあるかを事前に知る手段はないため、以下のいずれかの設計判断が必要です。

- 常に保守的な間隔(50ms程度)を採用する(実装は単純だが、長い文章では明確な遅延になる — 40文字で2秒)。
- 読み返し・エラー・タイムアウト等をもとに間隔を動的に調整する(実装は複雑になるが、対応可能なコントロールでは高速に打ち込める)。
- 間隔を設定可能にし、運用側の判断に委ねる。

現時点ではいずれも実装しておらず、3W-1-cのPoCは50ms固定のまま3W-1-dへ進んでいます。

### 14. 検証機で、UDPループバック(127.0.0.1 / ::1)が通らず、実QUIC接続を張る統合テストが実行できない(環境要因)

3W-1-d-1(入力メッセージ型・`input_session.rs`の追加)の際に判明した、コードではなく検証機側の問題です。`git stash`で変更前のコミット(`8513830`)に戻しても`tests/conn_establish.rs`が同じ「client-side QUIC handshake: TimedOut」で失敗するため、SARDPの変更とは無関係です。

切り分け結果(2026-09-12):

- `quinn`やTLSを介さない、同一プロセス内で送受信を完結させる素のUDPループバック(`System.Net.Sockets.UdpClient`)で、`127.0.0.1`宛・`::1`宛のいずれもパケットが届かない(受信側がタイムアウト)。
- 同じ場所での素のTCPループバック(`TcpListener`/`TcpClient`、`127.0.0.1`)は**通る**。
- Discordの画面共有を止めても変化なし。
- 環境: Tailscale VPN(`Tailscale Tunnel`アダプタ)稼働中、Hyper-V仮想アダプタ(`vEthernet (WSL (Hyper-V firewall))`、`vEthernet (Default Switch)`)あり、AVはWindows Defenderのみ。Hyper-Vファイアウォール設定(`Get-NetFirewallHyperVVMSetting`/`Get-NetFirewallHyperVProfile`)は全て`NotConfigured`。ファイアウォールのブロックログは無効で、`netsh wfp`からもUDPを塞ぐフィルタは見つからなかった(非管理者のため網羅性は不明)。
- 時間を区切った切り分け(約15分)ではここまでで、根本原因は未特定。
- **ただし壊れているのはループバックだけ**: 同じ機の非ループバックのローカルアドレス宛(LANの`192.168.1.10`、Tailscaleの`100.83.176.98`、Hyper-V仮想アダプタの`172.x`)への同一プロセス内UDP送受信は**すべて通る**。
- **Tailscale切断でも再現、原因はTailscale以外**: `tailscale down`後(バックエンド`NoState`、Tailscaleアダプタは169.254のAPIPAアドレスのみで100.xなし)に同じ素のUDPループバックテストを実行しても、127.0.0.1/::1ともに失敗した。ただし非管理者のため`Tailscale`サービス自体は停止できておらず、アダプタ(とTailscaleが入れているWFPフィルタ)は残った状態での結果である点は留意。残る候補はHyper-V(WSL用ファイアウォール)、Defenderの何らかの機能、他の常駐ソフト。これ以上は深追いしない。

**影響**: `127.0.0.1`をハードコードしている統合テスト(`tests/conn_establish.rs`、`tests/m2_handshake.rs`等、M1〜M6由来のもの)はこの機ではそのままでは実行できません。`cargo test --lib`(ユニットテスト、310件)は通ります。

**回避策(採用)**:
- 3W-1-d-2のE2E疎通(`sardp-server --capture desktop --bind 192.168.1.10:4433` + `sardp-client --server 192.168.1.10:4433`)はこの方法で実施し、実デスクトップのハードウェアH.264フレーム34枚がクライアントで復号されることを確認済み(2026-09-12)。
- `tests/stage3w1d_input.rs`は環境変数`SARDP_TEST_BIND_ADDR`(IPv4)でバインド先を差し替えられるようにしてあり、`SARDP_TEST_BIND_ADDR=192.168.1.10`で実QUIC上の往復2件が成功することを確認済み(2026-09-12)。既存の統合テスト群の`loopback()`ヘルパーにも同じ差し替えを入れれば、この機で全て実行できるはずです(未実施)。
- `sardp-server --bind`/`sardp-client --server`は任意のアドレスを取れるため、3W-1-d-2以降の実機疎通は`127.0.0.1`の代わりにLAN IPを使えば、規範仕様5.2節のTCPフォールバック(現状`src/`に一切未実装: ChannelBind、TLS-Exporterによるproof、チャネル別TLS+TCP接続)を先に実装する迂回は不要です。

### 15. `sardp-client --display log`(フレーム毎`ffmpeg`復号)は実デスクトップ配信に追いつかず、バックプレッシャの世代リセットが多発する(3W-1-d-2の観察、d-3で永続デコーダ側は解決)

**d-3での状態(2026-09-12)**: `sardp-server --capture desktop`は既定で**IDR+Pフレーム**(世代オープン時に`request_idr`、安全網として60秒GOP)になり、`sardp-client --display window`(項目16)なら2560x1440@59fpsを世代リセットなしで表示できます。以下の観察は`--display log`(フレーム毎`ffmpeg`起動、自己完結フレームしか復号できない)に`sardp-server --all-idr`を組み合わせた場合に今も再現する挙動で、そちらはM1〜M6由来のタイムコード検証用経路として残しているだけです。d-3では`sardp-client`が世代リセット時に新しいInstanceを受け直すようになった(受け直し中に再度リセットされても継続)ため、この経路でもセッションは終了せずリセットを繰り返しながら続きます(10秒で10世代、`captures/stage3w1d3_logmode_*`)。

d-2時点の`sardp-server --capture desktop`は、既存クライアントがフレームごとに新しい`ffmpeg`プロセスで独立に復号する設計のままでも動くように、**意図的に全フレームをIDR**(GOP=1 + 毎入力`ForceKeyFrame`)にしていました(現在は`--all-idr`オプション)。

d-2のE2E疎通(項目14参照)での観察:

- サーバー側の`capture_ts→encode_done_ts`は**11〜19ms**(現行PoCのフレーム毎`ffmpeg`起動方式は150〜300ms、DR-036)。
- クライアントは2560x1440のIDRを1枚ずつ`ffmpeg`で復号するため(数百ms/枚)、59fpsの供給に追いつけません。サーバー側は有界チャネル(容量4)で**ソース側ドロップ**(DR-007)し、25秒で256枚以上を破棄。それでも`client_queue_delay_us`が増え続け、**バックプレッシャのハード閾値超過→世代リセット→再オープン**が6回発生しました(再オープン経路と`DesktopH264Source::request_idr`が実機で動いた確認にはなっています)。
- (d-2時点)既存の`sardp-client`は世代リセット(`Video(Read(Read(Reset(0))))`)でセッションを終了していました。**d-3で解消**(上記)。

このほか、実装中に判明したMFT側の挙動(3W-1-bのREADMEの追記事項):

- `CODECAPI_AVEncMPVGOPSize`は`SetOutputType`より**後**に設定すると無視されました(先に設定すれば効く)。
- `MFSampleExtension_CleanPoint`は最初のIDRにしか立たなかったため、IDR判定はNAL種別(type 5)の走査で行っています。
- ハードウェアMFTはSPS/PPSを最初のIDRにしか付けないため、最初のサンプルから抽出してキャッシュし、以後のIDRに前置きしています(`MF_MT_MPEG_SEQUENCE_HEADER`はフォールバック)。
- 開始直後の最初のIDRが極端に小さい(745〜767バイト)件は、**d-3で原因を特定し解消**しました。`sardp-client --dump-frames`で取り出して`ffmpeg`で復号すると2560x1440の**完全な黒画面**で、サーバー側の`DXGI_OUTDUPL_FRAME_INFO`を見ると`DuplicateOutput`直後の最初の`AcquireNextFrame`は`LastPresentTime=0, AccumulatedFrames=0`(デスクトップ画像を伴わない通知フレーム)でした。`LastPresentTime == 0`のフレーム(ポインタのみの更新も同じ)を最初の1枚を含めて常にスキップするよう変更し、最初のIDRが実画面(約190KB)になることを確認済み(`captures/stage3w1d3_final_*/dump/gen0_frame0_idr.png`)。なおエンコーダ入力テクスチャへのblit直後の`ID3D11DeviceContext::Flush`も入れましたが、こちら単独では黒画面は解消しなかった(原因はDXGI側)。副作用として、**完全に静止したデスクトップでは最初の実フレーム(=Instanceの最初のIDR)が次のデスクトップ更新まで待たされる**点が残ります(1秒経過で警告ログ、サーバー側は`SESSION_SETUP_TIMEOUT`15秒で打ち切り)。在席セッションでは実用上問題にならない想定ですが、静止画面での接続直後の挙動として記録しておきます。

### 16. 3W-1-d-3: `sardp-client --display window`(Windows、永続ハードウェアデコーダ+ウィンドウ表示)で分かったこと

`sardp-win::H264DisplayWindow`(`sardp-win/src/display.rs`)は専用スレッドでWin32ウィンドウ・D3D11デバイス・フリップモデルのスワップチェーン・Microsoft H264 Video Decoder MFT(D3D11デバイスマネージャ経由のDXVA、出力はNV12テクスチャ)を持ち、`ID3D11VideoProcessor`でNV12→バックバッファへ直接blitして表示します。CPU側の画素コピーはありません。E2E(`captures/stage3w1d3_final_*`、LAN経由、30秒)の結果は、2560x1440のIDR+Pフレーム配信を**1754枚表示・世代リセット0回**、定常状態でキュー待ち約25µs・デコード0.4〜0.5ms・表示0.1〜0.2ms(いずれもクライアント側計測、`client.stdout.log`)。実装中に踏んだ点:

- **`VideoFrameReader::read_next_frame`がキャンセル安全でなかった**(SARDP本体側のバグ、`src/video_session.rs`で修正)。ヘッダとペイロードを2回の`read_envelope`で読むため、`tokio::select!`の別アームが先に完了して途中でdropされると、次回の呼び出しがペイロードをヘッダとして読んで`PROTOCOL_UNEXPECTED_MESSAGE`になっていました。d-2までは`select!`の他アームが滅多に完了しなかったため顕在化せず、d-3でウィンドウのタイミング報告アームが毎フレーム完了するようになって発覚。読み終えたヘッダを`pending_header`に退避する形で修正し、`tests/stage3w1d_video_reader.rs`で回帰テスト化(`SARDP_TEST_BIND_ADDR`対応)。
- **Microsoft H264 Video Decoder MFTは低遅延モードを指定しないと出力を溜め込む**: 指定なしでは全フレームで`MF_E_TRANSFORM_NEED_MORE_INPUT`が返り続け、1枚も出ませんでした(Bフレームなしのストリームでも)。`IMFTransform::GetAttributes()`に`MF_LOW_LATENCY=1`を設定して解消。同じGUIDの`ICodecAPI`(`CODECAPI_AVLowLatencyMode`)経由は`VT_UI4`型を要求します(`VT_BOOL`は`E_INVALIDARG`)。
- **表示スレッドの`Present`をvsyncでブロックさせてはいけない**: `Present(1)`(および通常のフリップモデルでの`Present(0)`)は実測で約16ms/フレーム(=リフレッシュ間隔)ブロックし、供給レート59fpsと等しいため、デコーダ起動時の遅れ(約100ms)が永遠に解消されず、バックプレッシャのリセットが1秒ごとに繰り返されました。`DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`+最大フレームレイテンシ1のスワップチェーンにし、待機オブジェクトを`WaitForSingleObject(…, 0)`でポーリングして、受け付け可能なときだけblit+`Present(0)`、そうでなければデコード済み・未表示(参照フレームは維持)にする方式に変更。30秒で162枚が未表示(`not_presented`)になりましたが、これは受信バーストで復号がリフレッシュより先行した分で、リセットは0回です。
- **サーバー側の供給が59Hzディスプレイで約120フレーム/秒になっていた**: DXGI Desktop Duplicationはポインタのみの更新(`LastPresentTime == 0`)でもフレームを返すため、d-2の`sardp-win`はそれも全てエンコードしていました。ポインタのみのフレームをスキップし、さらにリフレッシュ間隔(`EncoderConfig.max_fps`と同じ値)で明示的にペーシングするよう変更(`captures/stage3w1d3_e2e_20260912_041834`が変更前、`042402`以降が変更後)。
- **クライアント側でフレームを落としてはいけない**: 最初の実装はデコーダキューが溢れたらフレームを捨てて「次のIDRまで待つ」設計でしたが、Pフレーム配信で受信済みフレームを間引くと、送受信は成功しているのにデコーダの参照チェーンだけが壊れるという新種の不整合になります(DR-019以来の「参照チェーンを壊さない」方針に反する)。仕様上はクライアント→サーバーの`KeyframeRequest{reason: DECODE_ERROR}`(2.10節、feedbackストリーム)でIDRを要求できますが、これを日常的な詰まり対策に使うと、DR-029で調整したサーバー主導のバックプレッシャ判定(ベースライン相対delta、ヒステリシス)と競合する第二のトリガー経路を作ることになるため採用していません。現在は無制限キューにして、遅延は`client_queue_delay_us`としてサーバーへ報告し、仕様2.10のバックプレッシャ(リセット→新世代=IDR)に任せます。新しい世代が来たら表示スレッドは古い世代の残りをスキップします。
  - **残るリスク(未対応)**: 無制限キューのため、サーバー側のバックプレッシャが反応するまで(往復+閾値判定の時間)はクライアントのメモリ使用量に理論上の上限がありません。30秒の実測では定常状態のキュー待ちは約25µsで健全でした。将来的には、キューが一定の閾値(数百ms相当)を超えたら通常のバックプレッシャとは別に**緊急避難として`KeyframeRequest{DECODE_ERROR}`を1回だけ送るサーキットブレーカー**を設けるのが妥当です(`KeyframeRequest`自体は`src/messages.rs`に未実装)。
- **`VideoConverter::convert`(サーバー側、d-2)で`ID3D11VideoProcessorInputView`の参照が毎フレーム1つリークしていた**(`ManuallyDrop`で渡したCOMポインタを解放していなかった)。d-3で修正。
- **未対応のまま残っている点**: ウィンドウのリサイズ非対応(固定サイズ、`--window-size`で指定)、ストリーム解像度の変更はデコーダを作り直すだけで未検証、`--display window`はWindows専用(他OSは`--display log`のみ)、Tier 4(ソフトウェア)デコーダへのフォールバックなし(DXVA非対応MFTだと`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`チェックで起動失敗する)。

### 17. 3W-1-d-4: 入力注入(`input`ストリーム → `SendInput`)の結線で決めたこと・残したこと

`sardp-client --display window --input on`がウィンドウのキーボード・マウス入力を仕様2.12のメッセージにして`input`ストリームで送り、`sardp-server --capture desktop`が`sardp-win::InputInjector`(専用スレッド、`SendInput`)で注入します(合成キャプチャのサーバーはログのみ。開発機で合成テストを走らせたときに勝手にキーボード・マウスを動かさないため)。E2E(`captures/stage3w1d4_e2e_*`、同一機・LAN経由)は、ミラー経由のクリックでメモ帳へフォーカス移動(実カーソル位置の誤差1px)、`TextInput`31文字(日本語含む)+`KeyEvent`(Enter)がメモ帳に届くことを`WM_GETTEXT`の読み返しで確認済み(`tools/dxgi-capture-poc/src/bin/input_e2e_driver.rs`)。

- **文字を生む物理キーの`KeyEvent`はCLIENT_SIDEモードでは注入しない**(`sardp::input_state::should_inject_key`): 仕様2.12「文字生成は`TextInput`に基づかなければならない」の裏返しで、印字キーのスキャンコードを注入すると相手OSがもう一度文字を生成して二重入力になります。Ctrl/Alt/Winが押されたショートカット、修飾キー、Enter/Tab/Backspace/Escape、ナビゲーション・ファンクションキーは物理キーとして注入。クライアント側は`WM_CHAR`の制御文字(Enter等、Ctrl+英字)を`TextInput`から除外して整合させています。REMOTE_SIDEでは全て物理キー。キーパッドの数字はNumLock状態で挙動が変わりますが、文字キー扱い(注入しない)に倒しています。
- **HID Usage ↔ Windowsスキャンコード表**(`sardp-win/src/keymap.rs`、約120キー、JIS配列の変換/無変換/カタカナ/¥/ろ含む)。Pause(HID 0x48)は`lParam`上NumLockと同じコード(0x45)で区別できないため未対応。メディアキー等も未対応(`unmapped key`としてトレースのみ)。
- **同一機でのエコー防止**: 注入イベントの`dwExtraInfo`に固定タグ(`INJECTED_EXTRA_INFO`)を入れ、クライアントウィンドウは`GetMessageExtraInfo()`が一致する入力を転送しない(メッセージポンプでは`TranslateMessage`も抑止)。E2Eドライバはこのため`SendInput`ではなく`PostMessage`でクライアントウィンドウに入力を届けています。別機構成では不要ですが害もありません。
- **押下状態の不変条件(仕様4.4.2)**: サーバーは注入したキー・ボタンの押下集合を持ち、`input`ストリーム終了時とセッション終了時に解放を合成して注入。クライアント側も同じ集合を持ち、ウィンドウのフォーカス喪失・終了時に解放イベントを送ります(リモート側のキーが押しっぱなしにならないように)。
- **`TextInput`の文字間隔はサーバー側`--text-char-delay-ms`(既定50ms)**: 項目13の「保守的な固定値+設定可能」の組み合わせ。31文字で約1.5秒。
- **クライアント起点の複数`accept_uni`が競合する問題**: サーバーの`run_active_session`には既に`audio_capture`の`accept_uni()`アームがあり、そこへ`input`用のアームを足すと同じストリームを取り合って互いに`WrongStreamKind`で落とす構造でした。`accept_incoming_uni`で1箇所で受けてプロローグの`kind`で振り分ける形に変更(`audio_session::accept_audio_capture_from_reader`、`InputReceiver::from_reader`)。
- **IME**: `ImeComposition`(未確定文字列、`WM_IME_COMPOSITION`の`GCS_COMPSTR`)は送信・受信・ログまでで、サーバー側の注入は未実装(確定文字列は`WM_CHAR`経由で`TextInput`として届くので実用上は打てる)。`ImeModeChange`はクライアントに送る手段(UI)がなく、サーバー側SM(`ImeModeSm`)のユニットテストのみ。
- **権限の途中変更**: サーバーは`grant-keyboard`/`revoke-keyboard`/`grant-mouse`/`revoke-mouse`の管理コマンドで即時に注入を止められます(`PermissionSm`をイベントごとに参照)。クライアントは接続時点の付与状況でしか送信可否を決めず、後からの付与には反応しません(AUDIO_CAPTUREと同じPoCの割り切り)。E2Eでの動作確認は未実施(ユニットテストとコードレビューのみ)。
- **未対応**: `Wheel.is_precise`(常にfalse)、マウスの相対移動モード、Alt+Tab等ローカルOSが横取りするキー、Secure Desktop(3W-2)、Pauseキー。

### 18. Stage 3 遅延の再実測(DR-036)で分かったこと: TimeSync の 1 往復は信頼できない、エンコーダ出力の 1 フレーム遅れ

3W-1 完了後に M6 の指標(`transport_us` / `glass_to_glass_us`)を永続パイプラインで再計測しました。結果と方法は `docs/sardp-stage3-latency-measurements.md` にまとめています(要点: `ffmpeg` 起動 ≈190ms → NVENC+DXVA ≈7ms、同一機 LAN 経由の glass-to-glass 中央値 177ms → 9ms、定常 p95 15〜61ms)。計測の過程で見つけて直した実装上の問題が 2 つあります。

- **TimeSync(仕様 2.9)の 1 往復実装はオフセットが数百 ms ずれることがある**: 同一機でも握手直後の最初の往復で RTT 300〜600ms が観測され(2 往復目以降は 1ms 未満。原因は未特定)、その 1 サンプルから求めたオフセットで換算した値は全て無意味でした。これはバックプレッシャの主信号 `client_queue_delay_us`(仕様 2.10)も同じオフセットを使うため、**実運用でも判定を狂わせ得る問題**です。`timesync::client_time_sync` は 8 往復して最小 RTT のサンプルを採用(`best_of`)、サーバーは接続確立時に連続する要求をまとめて答え(`server_respond_time_sync_burst`、200ms の追従猶予)、以後は control ループでも `TimeSyncRequest` に応答するようにしました。仕様 2.9 自体は往復回数を規定していないので仕様変更はしていませんが、実装要件(Part 8)に「複数往復・最小 RTT 採用を SHOULD」として書く価値はあります(未反映)。
- **NVENC MFT の出力を次フレームの入力時まで回収していなかった**: `encode_frame` が入力直後に「既に準備できている出力」しか取らず、フレーム N の出力はフレーム N+1 の入力時(約 17ms 後)に届いていました。1 キャプチャ間隔を上限に「この入力の出力」を待つ形に変更し、encode の実測は avg 28ms → 7〜10ms、配信遅延も実際に 1 フレーム分縮んでいます。待機中に `METransformNeedInput` を読み捨てると次の入力で 5 秒タイムアウトになる(最初の版で発生)ため、クレジットとして保持します。
- **未対応**: 立ち上がり 0.7 秒の受信バッファ滞留(クライアントがデコーダ・ウィンドウを作る間に溜まる)、別機・実ネットワークでの再計測、`--display log` 経路は依然 `ffmpeg` 起動込みで 100ms 超/フレーム(比較用に残置)。

### 19. クレート構成の変更: バイナリを `sardp-cli` に分離し、共有ロジックを `sardp` に集約(3M-1 の準備)

macOS 実装の前に、`sardp-win` にあった OS 非依存の部分を `sardp` に移しました: `frame_source`(`EncodedFrame`/`SourceInfo`/`DesktopH264Config`、`FrameWorker`(ワーカースレッド+準備完了待ち+停止/join)と `FrameSender`(有界チャネル+`try_send` によるソース側ドロップ、DR-007))、`h264`(NAL 種別走査、`is_idr_access_unit`、`ParameterSetCache` による SPS/PPS 補完)。`sardp-win` はこれらを使う側になり、ユニットテスト(NAL 5 件、ワーカー 6 件)は `cargo test --lib` で OS を問わず走ります。

このとき `sardp`(コア)が `sardp-win` に依存していた(`sardp-server`/`sardp-client` がコアクレートの `src/bin` にあったため)ことが循環依存になるので、**バイナリを新パッケージ `sardp-cli/` に移動**しました。`target/debug/sardp-server.exe` 等のパスは変わりません。ワークスペースの `default-members` は `.`(コア)と `sardp-cli` で、Windows 専用クレートは `--workspace` または `-p` で明示ビルドします(Linux でもコア+バイナリはそのままビルド・テストできる構成)。この機では Linux 向けの `cargo check --target x86_64-unknown-linux-gnu` が `ring` の C コンパイラ要件で止まるため、Linux 上での実行確認は未実施です(移した箇所に `cfg` 依存はありません)。

## Stage 3(macOS OS統合、`tools/sck-capture-poc`)関連

3M-1-a(ScreenCaptureKit 疎通)で判明した、SARDP 本体ではなく macOS の TCC(Transparency,
Consent and Control)側の既知事項です。検証機は macOS 26.6.2 (25G83) / Apple M4、
Command Line Tools のみ(Xcode なし)、Apple 発行のコード署名 ID は 0 件。

### 20. macOS の画面収録許可はモーダルを出さない。システム設定の一覧への自動登録が唯一の在席経路

`.app` を `open` で起動して(責任プロセスをアプリ自身にして)画面収録を要求しても、
`CGRequestScreenCaptureAccess()` は即座に false を返し、SCK は 30ms ほどで
`SCStreamErrorDomain Code=-3801` で失敗します。tccd のログでは `auth_reason=5`(Service Policy)
で、`display_prompt:` は**一度も呼ばれません**。代わりに
`Notifying for access kTCCServiceScreenCapture ... to UID: 501` が出て、アプリが
システム設定 > プライバシーとセキュリティ > 画面とシステムオーディオの収録 の一覧に
**未チェックで自動登録**されます。ユーザーがチェックを入れると `Allowed (System Set)` が書かれ、
以後キャプチャできます。ad-hoc 署名・自己署名いずれでも同じです。

当初は「Apple 発行の信頼された証明書がないアプリには TCC がプロンプトを出さない」と考えましたが、
**これは反証済み**です。同じ自己署名 ID の `.app` から `AVCaptureDevice.requestAccess(for: .audio)`
を呼ぶと tccd は普通にプロンプトを出します(`display_prompt: called for ... kTCCServiceMicrophone`
→ `CFUserNotification response: 0x0`)。同一 identity・同一署名・同一配置でサービスだけ変えて
挙動が変わる以上、これはコード署名の属性ではなく **(identity, service) の組**に対する判定です。
`NSScreenCaptureUsageDescription` の欠落(それなら `auth_reason=8`)、Hardened Runtime、
`/Applications` 配下かどうか、MDM・Screen Time ポリシーはいずれも除外済み。詳細と切り分けの
全量は `tools/sck-capture-poc/README.md` にあります。

- **未確定**: Developer ID で notarize したアプリなら画面収録でもモーダルが出るのか。証明書なしで
  反証できます — notarize 済みの第三者アプリを `tccutil reset ScreenCapture <bundle id>` してから
  画面収録を要求させ、`display_prompt: called for ... kTCCServiceScreenCapture` が出るかを見る。
  出れば identity 依存、出なければ service 依存で確定します。**受け入れ基準「TCC 権限がゼロの
  状態からの初回起動フローが破綻しない」は、最終的な署名形態(notarize 済み配布物)で再確認が必要**。
- **検証の残骸**: 切り分けに使った `SwiftTccProbe`(`io.sardp.swift-tcc-probe`、`/private/tmp` 配下)が
  アクセシビリティの一覧に未チェックで残っています。実体が消えているため `tccutil reset` は
  "No such bundle identifier" で通りません。システム設定の「−」で削除する必要があります。

### 21. TCC の許可はコード署名にピン留めされる。ad-hoc での再署名は既存の許可を破壊する

ad-hoc 署名の指定要件は `identifier ... and cdhash H"..."` で、ビルドのたびに変わります。さらに
**証明書署名で許可を得た後に ad-hoc で再署名すると、tccd の `UpdateVerifierData` が保存済みの
csreq をその時の cdhash へ書き換え**、次のリビルドで
`Failed to match existing code requirement` となって許可が失われます(一覧にはチェック済みで
残るのに効かない、という分かりにくい壊れ方をします)。検証中に一度これを踏みました。

対策として自己署名証明書 "SARDP Dev Signing" を使い、指定要件を
`identifier "io.sardp.sck-capture-poc" and certificate leaf = H"0f7e..."` にしています
(未信頼のままでも `codesign` は通る)。これはリビルドで変わらないことを実測で確認済み
(cdhash が変わっても `CGPreflightScreenCaptureAccess = true` のままキャプチャできた)。
`tools/sck-capture-poc/make-app.sh` は既定でこの証明書を使い、キーチェーンに無ければ失敗します。

配布時の含意: **署名を変えると既存ユーザーの許可が全て失われます**。証明書更新やビルド方式の
変更時は、ユーザーに再許可を求める導線が要ります。

### 22. 実行中のプロセスは画面収録許可の付与を検知できない(再起動が必要)

`CGPreflightScreenCaptureAccess` を 2 秒ごとにポーリングし続けても、ユーザーがシステム設定で
チェックを入れた後に true へ変わりませんでした(10 分待って確認。許可自体は tccd のログで
`Update Access Record: ... to Allowed (System Set)` として書かれている)。プロセスを起動し直すと
即座に true になります。

sardp-server 側の設計要件として:

- 権限状態は Granted / Denied / Unknown の 3 値で表現する。
- Denied のときは `open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"`
  で該当ペインを開いて誘導する(モーダルは当てにできない、20 番)。
- **「権限待ち」を終端状態にしない**。許可後の反映にはプロセスの再起動が必要なので、
  再起動を促すか、自分で exec し直す経路を用意する。

### 23. 3M-1-b: VideoToolbox のエンコードはプロセス外で走る。解放漏れを in-process の指標では検出できない

`VTCompressionSession` の実体は同一プロセス内ではなく **`VTEncoderXPCService`**(VideoToolbox.framework の
XPCServices)で動きます。エンコード中に確認:

```
17539 .../VideoToolbox.framework/Versions/A/XPCServices/VTEncoderXPCService.xpc/...
```

このため、3W-1 で使ったような「起動・停止を繰り返して RSS / スレッド数 / FD 数を見る」リーク検査は
**VideoToolbox 側の解放漏れに対して無力**です。実際に対照実験として
`VTCompressionSessionInvalidate` を意図的に外して 10 サイクル回しましたが、
RSS(約 33.9MB)・スレッド数(18)・FD 数(20)・`VTEncoderXPCService` のプロセス数
(いずれもピーク baseline+1、終了後 baseline に復帰)のすべてで**差が出ませんでした**。
ARC の解放だけでもセッションは畳まれているように見えます。

- `sardp-mac/examples/desktop_source.rs --cycles N` はこの検査を行いますが、上記のとおり
  **「各解放呼び出しが効いていること」の証明にはなりません**。証明できるのは
  「再起動を繰り返してもパイプラインが in-process の資源を溜めないこと」までです。
- `VTCompressionSessionInvalidate` はドキュメント上の teardown 手順なので、
  観測できないとしても無条件に呼んでいます(規約に従うのであって、この機が罰してくれるからではない)。
- **未対応**: XPC サービス側の資源を観測する手段(`footprint`/`vmmap` を
  `VTEncoderXPCService` に対して取る等)は未整備。長時間稼働での再検証が要ります。

### 24. 3M-1-b: エンコード遅延の実測 — 3420x2224 で p50 16〜18ms

`sardp-mac` の `DesktopH264Source` で計測した capture→encode-done(同一の単調時計):

| 条件 | p50 | p95 | 最大 |
| --- | --- | --- | --- |
| 3420x2224(内蔵ディスプレイのネイティブ)、8Mbps、High profile | 16〜18ms | 22〜26ms | 31〜39ms |

Windows(NVENC)の 7〜10ms より大きいですが、画素数が約 3.7 倍(7.6MP)であることを考えると
概ね妥当です。フレーム間隔が 100ms 以上空く場面でも遅延は 16ms 前後で一定だったので、
DR-036 で Windows が踏んだ「フレーム N の出力を N+1 の入力時まで回収しない」型の
1 フレーム遅れではなく、素のエンコード時間です。

含意: Part 8 の目標(LAN 50ms)に対してエンコードだけで 16ms を使います。3M-1-d で M6 の
測定ハーネスを回すときは、**ネイティブ解像度のまま配信するのか、`SCStreamConfiguration.width/height`
で縮小するのか**を判断材料込みで決める必要があります(現在の実装はネイティブ固定)。

### 25. `cargo:rustc-link-arg` は依存クレートに伝播しない(Swift ランタイムの rpath)

`tools/sck-capture-poc/build.rs` が出す `cargo:rustc-link-lib` / `-search` は依存クレートの
バイナリまで届きますが、`cargo:rustc-link-arg` は**それを出したパッケージにしか適用されません**。
Swift ランタイムは SDK の `.tbd` 経由でリンクされ、install name が `@rpath/libswift*.dylib` のため、
rpath が無いバイナリは起動時に落ちます:

```
dyld[16971]: Library not loaded: @rpath/libswift_Concurrency.dylib
  Reason: no LC_RPATH's found
```

`sardp-mac/build.rs` に `-Wl,-rpath,/usr/lib/swift` を 1 行足して解決しています。
**シムをリンクするバイナリクレートはすべてこの 1 行が要ります** — 3M-1-d で macOS を
`sardp-cli` に結線するときも同じです。

### 26. 3M-1-c: アクセシビリティは画面収録とは別のTCCゲートで、こちらはプロンプトが出る

`CGEvent`でイベントを注入するには**アクセシビリティ(kTCCServiceAccessibility)**の許可が要ります。
画面収録(kTCCServiceScreenCapture)とは完全に独立で、片方があってももう片方は要求し直しになります。

画面収録と違い(#20)、こちらは`AXIsProcessTrustedWithOptions`でプロンプトが出ます
(3M-1-a で`universalAccessAuthWarn`が461x181のダイアログを出すことを確認済み)。
ただし到達点は同じで、`Update Access Record: kTCCServiceAccessibility ... to Denied (System Set)`
が書かれてシステム設定の一覧に未チェックで載り、ユーザーがチェックを入れて許可されます。

- `InputInjector::start`は`AXIsProcessTrusted()`を見て**起動時に失敗**します。
  黙って落とされるイベントを投げ続けるより、呼び出し側が誘導できる形で失敗する方がよいという判断
  (`CGEvent.post`には「システムに捨てられた」を伝える経路がありません)。
  Windows の`SendInput`は権限不要なので、`sardp_win::InputInjector::start`が`Self`を返すのに対し
  macOS 版は`Result`を返します。この非対称は本質的なものです。
- **未計測**: 実行中のプロセスが許可の付与を検知できるか。画面収録では検知できないと実測しました(#22)が、
  アクセシビリティでは測っていません。起動し直せば効くことだけ確認済みです。

### 27. 3M-1-c: macOS の入力注入で、Windows と同じに書くと壊れるところ

`sardp-mac::inject`が`sardp_win::inject`の直訳になっていない理由。いずれも
`sardp-mac/examples/input_e2e.rs`がセッションイベントタップで実測して確認しています。

| 事象 | 実測 |
| --- | --- |
| **修飾キーは状態であって打鍵ではない** | Command↓→a↓と送っても、`a`のイベント自身が`flags`にCommandを持っていなければショートカットにならない。`InputState`が押下中の修飾キーを追跡し、**すべての**イベント(マウスも含む)にflagsを付ける。実測: `keyDown key=0x00 flags=0x20100008`(NonCoalesced\|Command\|左Commandデバイスビット) |
| **修飾キーは`flagsChanged`として届く** | Command の仮想キーコードで`keyDown`/`keyUp`をpostすると、macOS 側が`flagsChanged`(type 12, keycode 55)に変換して配送する。`flagsChanged`だけを見ているアプリにもちゃんと届く |
| **ボタンを押したままの移動はドラッグ** | `mouseMoved`をpostしても多くのアプリはドラッグしない。押下中のボタンに応じて`leftMouseDragged`等に変える必要がある。実測: 押下中の3回の移動がすべて`leftMouseDragged`、`mouseMoved`は0件 |
| **ダブルクリックにはクリック数が要る** | `leftMouseDown`を2回送るだけでは2回のシングルクリック。2回目に`kCGMouseEventClickState = 2`が要る。実測: 2回目が`click=2` |
| **座標はポイント、キャプチャはピクセル** | キャプチャは3420x2224px、`CGEvent`は1710x1112pt。`InjectorConfig.scale`で割る。実測: ピクセル(1710,1112)→ポイント(855.0,556.0)、ウィンドウサーバの報告値と一致 |
| **返ってくるflagsは送ったflagsではない** | ウィンドウサーバが`kCGEventFlagMaskNonCoalesced`(0x20000000)を必ずORする。flagsの比較はマスクしてから行うこと |
| **左右の修飾キーの区別** | `kCGEventFlagMask*`だけでは左右が区別できない。IOKitの`NX_DEVICEL*/R*`ビット(0x1〜0x2000)を併せて立てる |

**キーマップ**: `sardp-mac::keymap`はHID usage↔macOS仮想キーコード。macOSに対応キーが無いものは
意図的に未マップにしています(Print Screen / Scroll Lock / Pause / Application / F21-F24、
およびPC JISの カタカナひらがな・変換・無変換)。Apple JISキーボードは英数/かなをLANG2/LANG1
(HID 0x91/0x90)として報告するのでそちらをマップしており、PC JISの3キーを無理に割り当てると
双方向で曖昧になるため入れていません。

**検証を安全に回す仕掛け**: `input_e2e`はイベントタップを**フィルタモード**で開き、
自プロセスのタグ(`kCGEventSourceUserData` = `SARD`)が付いたイベントだけをアプリに届く前に破棄します。
タグの無いイベント(実ユーザーの入力)は必ず素通しです。カーソル移動だけは意図的に通し
(ピクセル→ポイント変換をウィンドウサーバ相手に検証する唯一の方法のため)、終了時に元の位置へ戻します。
このタグはWindowsの`dwExtraInfo`と同じ役割で、3M-1-d で同一マシンのループバックE2Eを組むときに
クライアント側がエコーループを断つのにも使います。

### 28. 3M-1-d: sardp-cli への結線。プラットフォーム分岐の置き場所と、権限が片方だけ無い状態

`sardp-server` は `#[cfg(windows)]` を macOS 用に複製するのではなく、**`use sardp_win as os` /
`use sardp_mac as os` の別名 1 箇所**にまとめました。`sardp-win` と `sardp-mac` が意図的に
同じ形の API(`DesktopH264Source`、`InjectCommand`、…)を出しているので、配線はほぼ 1 回書けば済みます。
分岐条件そのものは `sardp-cli/build.rs` が出す `desktop_capture` cfg で、Linux(3G-1)を足すときは
build.rs の 1 行だけです。

本当に違う 2 箇所だけを関数に閉じ込めています:

- `desktop_profile_tier()`: Windows は Main(77)、macOS は High(100)。ハードウェアエンコーダが
  実際に出すプロファイルが違うため。Tier はどちらも 3。
- `start_input_sink()`: `SendInput` は権限不要で失敗しない(`Self` を返す)のに対し、`CGEvent` は
  アクセシビリティが要る(`Result` を返す)。

**権限が片方だけ無い状態を設計上の状態として扱っています。** macOS では画面収録と
アクセシビリティが独立なので、「映像は出るが入力は注入できない」は実際に起こります。
その場合は接続を失敗させず `InputSink::Unavailable(reason)` にし、入力イベントが来たときに
理由を出します。`InputSink::Log`(合成映像)と区別しているのは、**黙って捨てると
クライアントのメッセージが届いていないように見える**からです。閲覧のみのセッションは
アクセシビリティが無くても有用なので、接続ごと落とすのは過剰と判断しました。

#### ループバック E2E の結果(2026-09-13)

macOS のクライアントには入力を生む GUI がまだ無いので、`sardp-client --input-script` を足しました
(移動 → クリック → ダブルクリック → ドラッグ → Shift+a → テキスト → ホイールの固定列)。
安全のため `sardp-mac/examples/input_guard.rs` を先に起動します。これは**フィルタモードの
イベントタップ**を持ち、`kCGEventSourceUserData` が `SARD` のイベントだけをアプリに届く前に
破棄します(タグの無い実ユーザー入力は素通し)。タグはイベント側に乗るのでプロセスをまたいで効きます。

```
tools/sck-capture-poc/make-app.sh --app-name SardpGuard  --package sardp-mac --example input_guard --no-run
tools/sck-capture-poc/make-app.sh --app-name SardpServer --package sardp-cli --bin sardp-server  --no-run
open -n --stdout guard.log  --stderr guard.log  target/debug/SardpGuard.app  --args --secs 120
open -n --stdout server.log --stderr server.log target/debug/SardpServer.app --args \
    --capture desktop --all-idr --bind 127.0.0.1:4455 --cert-out /tmp/cert.pem
./target/debug/sardp-client --server 127.0.0.1:4455 --trust-cert /tmp/cert.pem --input-script
```

サーバー側:

```
desktop capture started: 3420x2224 @ 60Hz, hardware H.264 profile 100, all-IDR (--all-idr)
input injection: CGEvent, output origin (0, 0) pt, scale 2, text char delay 50ms
input summary: injected=24 skipped_character_keys=2 dropped_not_granted=0 mouse_moves=4
encoder: inputs=4085 outputs=4085 dropped_by_encoder=0 dropped_by_consumer=48
connection ended cleanly
```

ガード側(実際に OS に届いたもの): 注入イベント 47 件を破棄、実ユーザーのイベント 363 件は素通し。
内訳が計算どおりに閉じます:

| 種別 | 件数 | 由来 |
| --- | --- | --- |
| mouseMoved | 5 | 明示的な移動 1 + ボタン押下前の位置決め 4 |
| leftMouseDragged | 7 | ドラッグ中の明示的移動 3 + ボタン解放前の位置決め 4(押下中なのでドラッグになる) |
| leftMouseDown / Up | 4 / 4 | クリック 3 + ドラッグ 1 |
| flagsChanged | 2 | Shift の押下/解放(修飾キーは `flagsChanged` として届く、#27) |
| keyDown / keyUp | 12 / 12 | テキスト "SARDP 3M-1-d" の 12 書記素クラスタ |
| scrollWheel | 1 | ホイール 1 ノッチ |

サーバーが `MouseButton` の直前に必ず位置決めの `MouseMove` を出す(「クライアントが見た場所に
クリックを落とす」ため)ので、ボタンが押されている間の位置決めが `leftMouseDragged` になります。
これは `sardp-mac::InputState` の「ボタン押下中の移動はドラッグ」規則がサーバーの実際の
イベント列に対して意図どおり働いていることの確認でもあります。

**`skipped_character_keys=2`**: スクリプトが送った `a` の押下/解放はサーバーが意図的に
注入していません。仕様 4.4.1 / DR-025 の「IME が CLIENT_SIDE のとき、文字は `TextInput` が
MUST の供給源で、合成中の生 `KeyEvent` を重ねて送ってはならない」に従った挙動です
(修飾キーの Shift は文字キーではないので注入されています)。E2E が偶然この規則を踏んで、
正しく効いていることが確認できました。

遅延の実測は `docs/sardp-stage3-latency-measurements.md` の macOS 節に記録しています。
要点: `transport` p50 6.6ms に対し `decode` p50 53.8ms が支配的で、これは
`--display log`(フレームごとの `ffmpeg` 起動)のコストです。**macOS 版の永続デコーダ・
クライアントが無いため、Windows の構成 A と同じ土俵での比較はまだできません。**

