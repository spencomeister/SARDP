# SARDP PoC 実装ブリーフ(Claude Code向け)

このファイルは `sardp-normative-spec-v0.3.md` を実装に落とすための**スコープ定義書**である。仕様書そのものではない。仕様の詳細は必ず本体ファイルを参照すること。本ブリーフは「何を検証したいか」「どこを省略してよいか」を明確にし、実装が仕様全体(認証・TCPフォールバック・音声・ファイル転送等)へ発散するのを防ぐことが目的。

## 0. 渡すファイル

- **`sardp-normative-spec-v0.3.md`(必須)**: 現行の規範仕様。実装はこれに従う。
- **`sardp-schema-v0.1.md`(任意)**: 設計経緯・棄却案の記録。「なぜこの仕様なのか」を知りたいときのみ参照。実装に必須ではない。
- `sardp-normative-spec-v0.2.md` は渡さない(supersededであり、混同すると古い定義で実装してしまうリスクがある)。

## 1. このPoCの目的

プロトコル全体の実装ではなく、**設計上の判断が実際に機能するかの検証**が目的。特に以下は議論を重ねて確定した部分であり、優先的に動作確認したい(仕様書中のDR番号を付す)。

- **DR-001, DR-007, DR-017**: 映像は全信頼性ストリーム+送信元ドロップ+generation管理によるストリーム再開で成立するか。
- **DR-029(最重要)**: バックプレッシャ判定をベースライン相対のdeltaに直した修正が正しく機能するか。具体的には、**高RTT・無輻輳の回線で誤ってCongested/Recoveringに入らないこと**を確認したい(絶対閾値だった旧設計ならここで誤発火していたはずの箇所)。
- **DR-019**: Bフレーム禁止により `frame_id` が送信順=デコード順=表示順で単調に保たれるか。
- **DR-024**: 最初の1モニターのVideoStream ChannelがLiveに達した時点でConnection SMがActiveに遷移する挙動。

## 2. PoCスコープ

### やること(実装対象)
- Part 2.1(共通規約)、2.1.1(Envelope)、2.2(StreamPrologue)
- Part 2.3のうち `ClientHello`/`ServerHello`/`AuthPubkey`/`AuthResult` のメッセージフレーム(下記「省略してよいこと」参照)
- Part 2.4(DisplayConfig、モニター1枚分)
- Part 2.9(TimeSync、KeepAlive)
- Part 2.10(映像一式: VideoStreamGeneration, EncoderConfig, VideoFrame, generation管理, バックプレッシャ)
- Part 2.14(TransportFeedback)
- Part 4.1(Connection SM。ただしSuspended/Reconnectionは最小限でよい)
- Part 4.3(VideoStream SM。Channel/Instance両層とも実装。ここが本PoCの核)
- Part 4.7のタイムアウト値(既に確定値が入っている。そのまま使う)
- Part 8の測定ハーネス方針(タイムコード埋め込み、E2E遅延計測、tc netem)

### 省略してよいこと(スタブ・未実装で可)
- **認証**: WebAuthn/Passkey実装は不要。固定の鍵ペア(テスト用に生成した1組)による`AuthPubkey`の署名検証のみ通せばよい。チャレンジ再利用禁止・試行回数制限は実装してもよいが必須ではない。
- **TCPフォールバック(Part 5.2)**: スキップ。QUICのみ。
- **音声(2.13)、クリップボード(2.7)、ファイル転送(2.6)**: スキップ。
- **マルチモニター**: 1画面のみ。`ActiveMonitor`/Paused状態は実装不要。
- **OSキャプチャ**: 実キャプチャは行わない。タイムコード(生成時刻)を焼き込んだ合成画像をフレームソースとする(Part 8で規定済みの方針)。
- **リレー、Wayland統合、USBリダイレクト**: 対象外。
- **Datagram縮退モード(2.14の第2段階)**: 対象外。信頼性ストリームのみでよい。

### 推奨スタック
- 言語: **Rust**。理由: Envelopeパーサーを手書き最小実装としファジング対象にする方針(DR-021)とRustのcargo-fuzzが相性が良く、仕様の過去の議論でも `quinn` を前提にしていた。
- QUIC: `quinn`
- H.264エンコード/デコード: PoC初期段階では `ffmpeg` CLIを子プロセスとして呼び出す形で十分(バインディングの統合コストを後回しにできる)。安定してから `ffmpeg-next` 等のバインディングに置き換えることを検討。
- TLS証明書: ループバック検証用に `rcgen` でテスト用CAと証明書を生成する。**証明書検証を丸ごと無効化する実装は避ける**(PoC用のテストCAを信頼させる形にし、検証ロジック自体は本番相当に保つ)。
- ネットワーク再現: `tc netem`(Linux)。Claude Codeの実行環境がLinuxでない場合は、Docker上のLinuxコンテナで実行することを検討。

## 3. 推奨マイルストーン

1. **M1**: Envelope/StreamPrologueのパーサー実装+ユニットテスト(不正なmagic/length/未知typeの扱いを含む)。
2. **M2**: QUIC接続確立、ClientHello/ServerHello/AuthPubkey/AuthResultの交換。Connection SMのHandshaking→Authenticating→Authenticated遷移。
3. **M3**: 単一videoストリームの開設。VideoStreamGeneration→EncoderConfig→自己完結IDRの送出。タイムコード埋め込み合成画像→ffmpeg経由H.264エンコード。
4. **M4**: クライアント側デコード・表示、TransportFeedback送出、TimeSyncによるoffset算出。Connection SMのActive遷移(DR-024)。
5. **M5(本PoCの核)**: バックプレッシャ機構。`VIDEO_QUEUE_BASELINE_WINDOW`のローリング最小値追跡、Streaming/Congested/Recovering遷移、generation増分とストリーム再開、クライアント側の旧generation破棄。
6. **M6**: 測定ハーネス(E2E遅延自動計測)。`tc netem`でのLAN/WAN/損失プロファイル再現。DR-029の検証(高RTT・無輻輳での誤発火なし)を含む。

## 4. 受け入れ基準

- LAN相当(遅延なし)で glass-to-glass 50ms以下、WAN相当(既定プロファイルは要相談、例: 80ms RTT)で150ms以下(Part 8の目標値)。
- 意図的な輻輳(帯域制限+バッファ膨張)を発生させた際、`client_queue_delay_us` のdeltaが閾値を超えて Congested→Recovering に遷移し、generationが増分し、クライアント側表示が破綻せず回復すること。
- 高RTT(例: 300ms付加)・無輻輳の条件で、Congested/Recoveringに誤って入らないこと(DR-029の検証)。
- `frame_id` が常に単調増加し、逆順・重複が発生しないこと。

## 5. Claude Codeへの引き渡し文(例)

以下をそのまま渡してよい。

> `sardp-normative-spec-v0.3.md` と `sardp-poc-brief.md` を読み込んで、ブリーフのスコープに従いRustでSARDPのPoCを実装してください。まずM1(Envelope/StreamPrologueパーサー)から着手し、各マイルストーン完了時点でテスト結果を報告してください。仕様書とブリーフで矛盾する記述があれば、ブリーフを優先しつつ理由を報告してください。
