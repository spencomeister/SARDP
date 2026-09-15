# Stage 3 遅延実測: 永続エンコーダ/デコーダ・パイプライン(DR-036 の再計測)

M6 の測定ハーネス(`sardp::measurement`、PoC ブリーフ Part 8「E2E遅延自動計測」)は、フレームごとに `ffmpeg` を子プロセスとして起動する PoC 段階の経路を計測し、`glass_to_glass_us` がコーデック起動コストに支配されること(エンコード 70〜90ms + デコード 60ms、`tests/m6_measurement.rs` のモジュールコメント)を明らかにした。DR-036 はこの起動コストを「各 OS の実キャプチャ経路の実装で副産物として解消される」と位置づけている。本文書は 3W-1 完了時点(2026-09-12)で、Windows の永続パイプライン(DXGI → NVENC MFT → QUIC → DXVA デコーダ MFT → スワップチェーン)を同じ指標で計測した記録である。

## 計測方法

- 計測点は `sardp-client` が全フレームについて出力する(`--display window` / `--display log` 共通、`sardp-cli/src/bin/sardp-client.rs` の `FrameStats`)。
  - `encode_us` = `VideoFrameHeader.encode_done_ts − capture_ts`(サーバー時計内)
  - `transport_us` = 受信完了時刻(クライアント時計 → TimeSync オフセットでサーバー時計へ換算) − `encode_done_ts`。QUIC 送受信・Envelope 分割・CBOR デコードに加え、**サーバー側チャネル待ち・クライアントの受信バッファ滞留**も含む
  - `queue_us` = 受信 → 表示スレッドが取り出すまで(window のみ)
  - `decode_us` = デコーダ本体(window: 取り出し → 復号完了、log: `ffmpeg` 起動込みの往復)
  - `present_us` = 復号完了 → `Present` 完了
  - `glass_to_glass_us` = 表示完了時刻(サーバー時計換算) − `capture_ts`。仕様 2.10 の `client_queue_delay_us` と同じ定義で、モニタのスキャンアウトは含まない
- 時計換算は TimeSync(仕様 2.9)のオフセットに依存する。当初の 1 往復実装では同一機でも RTT が 300〜600ms と計測され、オフセット誤差で `transport`/`glass_to_glass` が 0 に張り付いた(下記「計測中に直したこと」)。現在は 8 往復の最小 RTT サンプルを採用し、RTT 0.4〜0.7ms・オフセット誤差 ±0.3ms 程度。
- 環境: 同一機(Windows 11、RTX 3060 Ti、2560x1440@59Hz)、LAN アドレス `192.168.1.10` 経由(UDP ループバックが使えないため、KNOWN_ISSUES #14)。サーバーとクライアントが同じ GPU・同じ CPU を使い、クライアントウィンドウ自身もキャプチャ対象に映り込む(再帰ミラー)。別機構成での数値ではない。
- 生ログ: `tools/dxgi-capture-poc/captures/stage3_latency_*/client.stdout.log`(gitignore 対象、第三者確認用に手元保持)。集計は先頭 N フレームを除いた `avg / p50 / p95 / max`。

## 結果

### 構成ごとの比較(2026-09-12)

| 構成 | encode | transport | decode | glass_to_glass | 備考 |
|---|---|---|---|---|---|
| **B. M6 当時の経路**: 合成フレーム + `ffmpeg` エンコード(サーバー、640x360@4fps)+ `ffmpeg` デコード(クライアント) | avg 109ms / p50 99ms / p95 136ms | avg 2.9ms / p50 1.2ms | avg 83ms / p50 75ms / p95 121ms | **avg 195ms / p50 177ms / p95 276ms** | n=45、12秒。M6 の「エンコード 70〜90ms + デコード 60ms」と整合(本機は少し遅い) |
| **C. HW エンコード + `ffmpeg` デコード**: `--capture desktop --all-idr` + `--display log`(2560x1440) | avg 6.4ms / p50 5.3ms / p95 11.8ms | avg 416ms(※) | avg 119ms / p50 109ms / p95 157ms | avg 542ms(※) | n=90、12秒。※デコードが供給(59fps)に追いつかずバックプレッシャの世代リセットが 11 回。`transport` はサーバー側チャネル待ちが支配し、ワイヤの数値ではない |
| **A. 永続パイプライン**: `--capture desktop`(IDR+P)+ `--display window`、run 1、全 1344 フレーム(30秒) | avg 9.8ms / p50 6.2ms / p95 24.9ms | avg 14.6ms / p50 1.7ms / p95 28ms | avg 0.75ms / p50 0.51ms / p95 1.5ms | avg 27.7ms / **p50 9.0ms** / p95 90ms / max 955ms | 立ち上がりの滞留込み(下記) |
| A. run 1、定常状態(先頭 300 フレーム除外、n=1044) | avg 10.6ms / p50 6.4ms / p95 36ms | avg 4.2ms / p50 1.7ms / p95 12ms | avg 0.72ms / p50 0.50ms / p95 1.4ms | avg 16.6ms / **p50 9.0ms / p95 61ms** / max 296ms | 画面に動きが多かった回(22.9MB/30秒)。p95 は encode の揺れ(p95 36ms)が主因 |
| A. run 2、定常状態(先頭 300 フレーム除外、n=830) | avg 7.0ms / p50 5.6ms / p95 11.7ms | avg 2.5ms / p50 1.7ms / p95 4.1ms | avg 0.60ms / p50 0.47ms / p95 1.2ms | avg 10.5ms / **p50 9.1ms / p95 15.0ms** / max 118ms | 画面がほぼ静止(19.6MB/30秒、pointer_only 724) |

`present_us` はいずれも 0.2〜0.3ms(p95 0.5〜0.7ms)。`queue_us`(window)は定常で p50 31µs、p95 0.2〜1.6ms。

### macOS、永続デコーダ・ウィンドウ導入後(3M-1-e、2026-09-16)

3M-1-d時点の課題(「macOSにはまだ永続デコーダのクライアントが無い」)を解消した後の計測。
サーバーは`--capture desktop`(IDR+Pフレーム、`--all-idr`なし)、クライアントは新設の
`--display window`(`sardp_mac::display::H264DisplayWindow` — `VTDecompressionSession`永続デコーダ
+ `NSWindow`、Windows`H264DisplayWindow`と同じ形のAPI)。同一機LAN経由(`192.168.1.6`、
内蔵ディスプレイ3420x2224@60Hz)、約38秒・601フレーム、世代リセット0回・全フレーム
`presented=true`(構成Aの土俵にmacOSが初めて乗った計測)。

| 構成 | encode | transport | queue | decode | present | glass_to_glass |
|---|---|---|---|---|---|---|
| **A(macOS)**: `--capture desktop` + `--display window`、全601フレーム | avg 32.0ms / p50 33.7ms / p95 45.1ms | avg 2.5ms / p50 1.9ms / p95 2.6ms | avg 20.6ms / p50 0.04ms / p95 206.6ms | avg 3.7ms / p50 3.7ms / p95 4.3ms | avg 3.4ms / p50 3.2ms / p95 6.2ms | avg 62.2ms / **p50 48.4ms** / p95 233.0ms / max 272.6ms |
| A(macOS)、定常状態(先頭30フレーム除外、n=571) | avg 32.6ms / p50 34.4ms / p95 45.2ms | avg 1.9ms / p50 1.9ms / p95 2.6ms | avg 12.2ms / p50 0.04ms / p95 95.4ms | avg 3.7ms / p50 3.7ms / p95 4.3ms | avg 3.4ms / p50 3.2ms / p95 6.3ms | avg 53.8ms / **p50 47.8ms / p95 121.8ms** / max 272.6ms |

読み取れること(Windows構成Aとの対比、両者とも同一機・同一GPUでencode/decodeを共有する条件):

- **`decode`(p50 3.7ms)と`present`(p50 3.2ms)はどちらも軽い。** `VTDecompressionSession`の
  同期デコード(`VTDecodeFrameFlags`を空にすると出力コールバックが`VTDecompressionSessionDecodeFrame`
  の戻り前に必ず走る、`VtDecoder.swift`のポーリング不要設計)と、IOSurfaceを介した
  `CALayer.contents`直代入(CPUコピーなし)が効いている。Windows(DXVA+`ID3D11VideoProcessor`
  blit、decode p50 0.5ms・present p50 0.2〜0.3ms)と桁は近い。
- **`encode`がp50 34msと、3M-1-b単体計測(同条件・同解像度でp50 16〜18ms)からほぼ倍増した。**
  Windows側が「同一GPUをクライアントのデコード・表示と共有している影響」として観測したのと同種の
  現象で、Apple SiliconのUnified Memory上でVideoToolboxのエンコード(サーバー)とデコード
  (クライアント)を同一プロセス空間内の同一GPUに同時発行しているための競合と考えられる。
  Windows(NVENC単体p50 5〜7ms → 同居時p50 5.6〜6.4msとほぼ無変化)と比べ、macOSの方が
  競合の影響を強く受けている。**別機構成での切り分けが必要**(Windowsの「今後」節と同じ課題)。
- **`glass_to_glass`のp50 47.8msは、Part 8のLAN目標(50ms)にほぼ収まるが、Windows(構成A、
  p50 9.0ms)の約5倍。** 支配要因は`encode`(p50 34ms)であって`transport`(p50 1.9ms、ワイヤは
  健全)でも`decode`/`present`(合計p50 7ms未満)でもない。
- **`queue`のp95が95〜207msと大きい**(中央値は0.04msで健全)。画面の動きが多い区間で
  デコードスループットが供給に一時的に追いつかず、キューが伸びる場面があったと見られる
  (バックプレッシャの世代リセットは1回も発生していないため、閾値には達していない範囲)。
  Windows(定常p50 31µs、p95 0.2〜1.6ms)より一桁大きく、`encode`の遅さがバースト時に
  そのままキュー滞留として跳ね返っている可能性が高い(同じ根本原因の二次効果)。

### macOS(3M-1-d、2026-09-13)

同一機ループバック。サーバーは `--capture desktop --all-idr`(ScreenCaptureKit → VideoToolbox、
内蔵ディスプレイ 3420x2224@60Hz)、クライアントは `--display log`(フレームごとに `ffmpeg` を起動)。
**Windows の構成 A とは比較できません** — macOS にはまだ永続デコーダのクライアント
(`--display window` は Windows 専用)が無いため、測れるのは構成 C 相当だけです。

| 構成 | encode | transport | decode | glass_to_glass | 備考 |
|---|---|---|---|---|---|
| **C(macOS)**: `--capture desktop --all-idr` + `--display log`、定常(generation 4、n=3087) | avg 27.5ms / p50 28.0ms / p95 43.0ms | avg 17.0ms / p50 6.6ms / p95 48.3ms | avg 54.8ms / p50 53.8ms / p95 60.4ms | avg 99.3ms / **p50 95.8ms / p95 128.0ms** | 80〜86KiB/フレーム。世代リセット 4 回(約68秒) |
| C(macOS)、立ち上がり(generation 0、n=13) | p50 19.0ms | p50 960ms | p50 51.2ms | p50 1030ms | クライアントが `ffmpeg` 起動に追いつけず QUIC 受信バッファに滞留、直後に世代リセット |

読み取れること:

- **`transport` p50 6.6ms** はワイヤ側が問題ではないことを示しています(Part 8 の `transport_us`
  目標 LAN 50ms に対して十分)。p95 48ms はデコーダが追いつかないときのサーバー側チャネル待ちで、
  ワイヤの数値ではありません(Windows の構成 C と同じ事情)。
- **`decode` p50 53.8ms が支配的**です。これは `ffmpeg` 起動コストであって macOS の問題ではなく、
  永続デコーダのクライアント(macOS 版 `--display window` 相当)を書けば Windows の構成 A と
  同じく 1ms 未満になるはずです。**これが現在 glass-to-glass の半分以上を占めています。**
- **`encode` が単体計測の p50 16ms(3M-1-b)から p50 28ms に増えています。** 条件の違いは
  (a) `--all-idr`(全フレームが IDR)と (b) 同一機でクライアントが毎フレーム `ffmpeg` を起動している
  こと。generation 1〜2 では p50 16〜17ms で、負荷が積み上がるにつれ 28ms に上がりました。
  Windows 側の「同一 GPU をクライアントのデコード・表示と共有している影響」と同種の現象です。
  **別プロセス・別機での再計測が要ります。**

### DR-036 の結論

- **コーデック起動コスト(`ffmpeg` 起動 ≈ 105ms + 85ms ≈ 190ms)は、永続パイプラインでは encode 6ms + decode 0.5ms ≈ 7ms になった。** glass-to-glass の中央値は 177ms → 9ms。
- Part 8 の目標(LAN 50ms / WAN 150ms)に対して、同一機・LAN インターフェース経由という条件では **定常状態で p50 9ms、p95 15〜61ms**。静止画面では p95 も 50ms を下回り、動きの多い画面では encode の揺れ(NVENC の内容依存、および同一 GPU をクライアントのデコード・表示と共有している影響)で p95 が 50ms を超える回があった。別機・実ネットワークでの数値は未取得(3W-1 の範囲外、Stage 3 後半の課題)。
- 立ち上がり: 最初の約 40 フレーム(0.7秒)は `transport` が 630ms から 1 フレームあたり約 17ms ずつ減る形で大きい。クライアントが最初のフレーム受信後にデコーダ MFT とウィンドウを作る(合計 0.5〜0.6 秒)間、後続フレームが QUIC の受信バッファに溜まり、その後まとめて処理されるため。定常状態には影響しない。ウィンドウとデコーダを `accept_video_instance` の前に用意すればほぼ消せる(未対応)。

## 計測中に直したこと(実測しなければ見えなかった 2 件)

1. **エンコーダの出力回収が 1 フレーム遅れていた**(`sardp-win/src/desktop_h264.rs`、`Encoder::encode_frame`)。非同期 MFT に入力を渡した直後は「その時点で既に準備できている出力」しか回収していなかったため、NVENC の処理(5〜10ms)が終わる前に戻ってしまい、フレーム N の出力は次のキャプチャ(約 17ms 後)のフレーム N+1 の入力時に回収されていた。最初の計測で encode が avg 28ms / p50 33ms と出たのはこのため。入力後に「この入力の出力」を最大 1 キャプチャ間隔(最低 20ms)まで待つよう変更し、encode は avg 7〜10ms / p50 5.6〜6.4ms に。**これは計測誤差ではなく実際の配信遅延がフレーム 1 枚分(約 17ms)縮んだ**ことを意味する。待機中に届く `METransformNeedInput` を読み捨てると次の入力で 5 秒のタイムアウトになる(最初の実装で発生)ため、イベントをクレジットとして保持する。
2. **TimeSync が 1 往復だけだった**(`src/timesync.rs`)。同一機で RTT 300〜600ms という値が出ており(原因は特定していないが、握手直後の control ストリームの最初の往復で発生し、2 往復目以降は 1ms 未満)、オフセットが 150〜300ms ずれて `transport`/`glass_to_glass` が飽和演算で 0 になっていた。仕様 2.9 の `client_queue_delay_us`(バックプレッシャの主信号)も同じオフセットを使うので、**これはバックプレッシャ判定そのものに影響する実装上の問題**でもある。クライアントは 8 往復して最小 RTT のサンプルを採用(NTP と同じ考え方、`timesync::best_of`)、サーバーは接続確立時に連続する要求を続けて答え(`server_respond_time_sync_burst`)、以後 control ループでも要求に応答するよう変更。

## 今後

- 別機(実 LAN / WAN)での再計測。`tc netem` 相当の条件再現は M6 ハーネス側にあるが、実デスクトップ経路とは接続していない。
- 立ち上がり滞留の解消(デコーダ・ウィンドウの事前生成)。
- ~~macOS の永続デコーダ・クライアント(`--display window` 相当)~~ → 3M-1-e で対応済み(上記)。
- **macOS の `encode` を負荷から切り離した再計測**(別機、またはサーバー/クライアントを
  別プロセスグループ・別GPUコンテキストで分離できるか)。3M-1-e の計測で、同一機上で
  encode(サーバー)と decode(クライアント)を同時稼働させると `encode` がp50 16〜18ms(単体)
  からp50 34ms(同居)へほぼ倍増することが分かった。Windows(同居でもp50 5.6〜6.4msとほぼ無変化)
  より競合の影響が大きい理由の特定が必要。
