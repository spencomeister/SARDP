# sck-capture-poc (3M-1-a)

`docs/sardp-stage3-os-integration-roadmap.md`のmacOS 3M-1: 在席キャプチャ・エンコード基盤の
単体疎通確認。Windowsの`tools/dxgi-capture-poc`と同じ位置づけで、SARDP本体(`sardp`クレート)とは
接続しない独立したCargoプロジェクト(ワークスペースのメンバーだが`default-members`には含めない)。

## 方針: Swiftシム + C ABI

ScreenCaptureKit(SCK)はSwift/Objective-C前提のAPI(async関数、デリゲートプロトコル、
NSStringキーのCMSampleBuffer attachment)なので、ロードマップの判断点どおり**薄いSwiftシムを
書いてRustからFFIで呼ぶ**構成にした。

- `shim/SckShim.swift`: SCKに触れる唯一の場所。`@_cdecl`でC ABIの関数だけを公開する
  (`sardp_sck_preflight` / `sardp_sck_request_access` / `sardp_sck_main_display_info` /
  `sardp_sck_start` / `sardp_sck_stop`)。フレームはコールバックで、ピクセルバッファをロックした
  まま同期的に渡す(呼び出し側がコピーして返す)。SCK側の`SCStream`はセッションオブジェクトが
  所有し、`sardp_sck_stop`で必ず解放する。`sardp_sck_start`のタイムアウト経路では、遅れて成功した
  セッションを止めてから捨てる(3W-1の教訓1: 成功パス以外でもOS資源を解放する)。
- `build.rs`: `swiftc -emit-library -static`でシムを静的ライブラリにし、Swiftランタイム
  (`/usr/lib/swift`、SDKの`.tbd`経由)と各フレームワークをリンクする。Command Line Toolsだけで
  ビルドできる(Xcode不要)。
- `src/shim.rs`: 安全なRustラッパー。`Session`が`Drop`で`sardp_sck_stop`を呼ぶ。
- `src/main.rs`: PoC本体(下記)。

VideoToolbox(3M-1-b)も同じシムに載せる予定(C APIだが、CMSampleBuffer/CMFormatDescriptionの
扱いがSwift側で完結する方が短い)。CGEvent(3M-1-c)は純粋なC APIなのでRustから直接呼ぶ案も残す。

## バイナリ

- **`sck-capture-poc`**: 画面収録権限の確認(読み取り専用のpreflight → 要求 → 権限待ちの
  可視化ループ)の後、メインディスプレイをSCKでキャプチャし、連番BMP
  (`captures/session_<unixtime>/frame_NNNN.bmp`)と`frames.log`(SCKのフレームごとの
  status / dirtyRects / displayTime / contentScale)を書き出す。
  - `--frames N` 保存枚数(既定10)、`--fps N` 最小フレーム間隔の逆数(既定10)、
    `--no-request` 権限ダイアログを出さない、`--wait-secs N` 権限待ち上限、
    `--force-sck` CGのpreflight/要求を飛ばしてSCK自身に権限を判断させる、`--cursor`。
- **`make-app.sh`**: ビルド結果を最小の`.app`バンドルに包んで署名し、`open`で起動する。
  **TCCの検証はこの経路でのみ意味がある**(下記)。署名IDは既定で`SARDP Dev Signing`
  (自己署名、下記4節)。`SARDP_SIGN_IDENTITY=-`で明示的にad-hocにできるが、**ad-hoc署名は
  既存のTCC許可を破壊する**ので通常は使わない。

```bash
export PATH="$HOME/.local/share/mise/shims:$PATH"   # このマシンのRustはmise経由
cargo build -p sck-capture-poc
tools/sck-capture-poc/make-app.sh -- --frames 5          # .app化 → 署名 → open → ログ表示
tools/sck-capture-poc/make-app.sh --no-run              # ビルドと署名だけ
```

`captures/`は実画面を含むため`.gitignore`で除外している。

## TCC(画面収録)権限フローの確認結果(2026-09-13、macOS 26.6.2 / M4)

ロードマップの「最初に踏むこと」。結論:

- **画面収録の許可ダイアログ(モーダル)は出ない。** これは署名の問題ではなく
  `kTCCServiceScreenCapture`固有の挙動(2節)。
- **代わりにアプリがシステム設定の一覧へ自動登録され、ユーザーがチェックを入れれば許可される。**
  ad-hoc署名でも自己署名でも通る(3節)。在席(3M-1)の権限フローはこれで成立する。
- **許可はコード署名にピン留めされる。** 署名が変われば無効になり、ad-hocでの再署名は
  保存済みの要件を上書きして許可を壊す(5節)。

### 1. 責任プロセスの帰属(重要、再現性あり)

TCCは要求を「責任プロセス(responsible process)」に帰属させる。Claude Codeのシェルから
バイナリを直接起動すると、tccdのログ上の`Resp:`は`com.anthropic.claude-code`になり、
ダイアログも許可リストの項目も「Claude」名義になる。`.app`バンドルにして`open`で起動すると
ppid=1(launchd)で起動し、`Resp:`はアプリ自身になる(tccdの`BUNDLE_ATTRIBUTION`行で確認)。
本番のsardp-serverはLaunchAgentとして起動する想定なので、こちらが本番と同じ条件。

### 2. 画面収録だけプロンプトが出ない(署名は無関係)

`.app`+`open`で起動した`sck-capture-poc`、および切り分け用の純Swift製アプリの両方で:

- `CGPreflightScreenCaptureAccess()` = false → `CGRequestScreenCaptureAccess()` = false(即時)。
- `--force-sck`(SCK直): `SCShareableContent`が約30msで
  `SCStreamErrorDomain Code=-3801 "The user declined TCCs for application, window, display capture"`。
- tccdログ: `Handling access request to kTCCServiceScreenCapture ... ReqResult(Auth Right:
  Unknown (None), promptType: 1, DB Action:None)` の後、`auth_reason=5`(Service Policy)。
  `display_prompt:`は**一度も呼ばれない**。代わりに
  `Notifying for access kTCCServiceScreenCapture for target PID[...] to UID: 501` が出る
  — これがシステム設定の一覧に行を追加する経路(3節)。

**当初は「Apple発行の信頼された証明書がないアプリにはプロンプトを出さない」と考えたが、
これは反証された。** 同じ自己署名ID(`SARDP Dev Signing`、未信頼)で署名した`.app`から
`AVCaptureDevice.requestAccess(for: .audio)`を呼ぶと、tccdは普通にプロンプトを出す:

```
display_prompt: called for <private> for service kTCCServiceMicrophone
Using default User Notification level for: ... Sub:{io.sardp.mic-probe}
CFUserNotification response: 0x0; service kTCCServiceMicrophone and subject io.sardp.mic-probe
```

同一identity・同一署名・同一配置でサービスだけ変えるとプロンプトが出る以上、
`promptPolicy=0`はコード署名の属性ではなく**(identity, service)の組**に対する判定である
(tccdの`-[TCCDPlatformMacOS promptingPolicyForIdentity:accessingService:withAttributionChain:]`
はserviceを引数に取る)。

この反証により潰れた枝:

| 疑い | 判定 |
| --- | --- |
| `NSScreenCaptureUsageDescription`等の欠落 | 無関係。欠落なら`auth_reason=8`(MissingUsageString)であって5ではない |
| Hardened Runtime / entitlements | 無関係。マイクは同条件でプロンプトが出た |
| `/Applications`配下でないこと、LaunchServices登録 | 同上 |
| Gatekeeper / notarization | `spctl`は両アプリとも rejected だがマイクは通る |
| MDM・構成プロファイル | なし(`profiles status`) |
| Screen Time等のポリシー | なし(`com.apple.applicationaccess`ドメイン自体が存在しない) |
| ad-hoc署名がTCC非対応 | 否。tccdには`TCCDAdhocSignatureCache`があり設計上サポートされる |

**未確定**: Developer IDでnotarizeしたアプリなら画面収録でもモーダルが出るのか。
証明書なしで反証できる: notarize済みの第三者アプリ(Chrome等)の
`tccutil reset ScreenCapture <bundle id>`後に画面収録を要求させ、
`display_prompt: called for ... kTCCServiceScreenCapture`が出るかを見る。
出れば identity 依存、出なければ service 依存で確定する。

### 3. 実際の在席フロー(実証済み)

ダイアログは出ないが、**拒否された時点でアプリがシステム設定の一覧に未チェックで自動登録される**。
ユーザーがチェックを入れると許可される:

```
Update Access Record: kTCCServiceScreenCapture for io.sardp.sck-capture-poc to Allowed (System Set)
replayd: -[RPClient hasScreenCaptureAccess...] TCC Allow
```

3M-1-cのアクセシビリティでは`universalAccessAuthWarn`がダイアログ(461x181)を出し、
TCCに`Denied (System Set)`を書いてから同じ一覧に載る。プロンプトの有無は違うが、
**最終的にユーザーがシステム設定で有効化する**という到達点は同じ。

サーバ側の設計上の含意:

- 権限状態は Granted / Denied / Unknown の3値で表現し、Deniedのときは
  `open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"`
  で該当ペインを開いて誘導する(モーダルは当てにできない)。
- **実行中のプロセスは許可を検知できない。** `CGPreflightScreenCaptureAccess`を2秒ごとに
  ポーリングし続けても、許可後にtrueへ変わらなかった(10分待って確認)。許可の反映には
  プロセスの再起動が必要。sardp-serverは「権限待ち」を終端状態にせず、
  再起動を促すか自分でexecし直す設計にする。

### 4. 署名IDとTCCレコードの安定性

ad-hoc署名の指定要件(designated requirement)は`identifier ... and cdhash H"..."`で、
ビルドのたびに変わる。そのため自己署名のコード署名証明書 "SARDP Dev Signing" を
ログインキーチェーンに作り(`openssl req`→`security import`、未信頼のままでも`codesign`は通る)、
指定要件を`identifier "io.sardp.sck-capture-poc" and certificate leaf = H"0f7e..."`にした。
これはリビルドで変わらない(実測: ソースを変更してcdhashが`b841cbf3...`→`27caff6d...`と
変わった後も`CGPreflightScreenCaptureAccess = true`のままキャプチャできた)。
削除は `security delete-identity -c "SARDP Dev Signing"`。

**ad-hocでの再署名は既存の許可を破壊する。** 一度これを踏んだ: 証明書署名で許可を得た後に
`make-app.sh`をad-hoc既定で走らせたところ、tccdの`UpdateVerifierData`が保存済みcsreqを
その時のcdhashへ書き換え、次のリビルドで:

```
matchesCodeRequirement: ... cdhash H"78cc4a8c..."; status: -67050
Failed to match existing code requirement for subject io.sardp.sck-capture-poc
  and service kTCCServiceScreenCapture
```

となって許可が失われた(一覧にはチェック済みで残るのに効かない)。`make-app.sh`は既定で
証明書署名を使い、キーチェーンに無ければ理由を出して失敗するようにしてある。

### 5. キャプチャ疎通の結果

許可を得た状態で5フレーム取得できた(`--frames 5 --fps 10`、内蔵ディスプレイ 3420x2224px):

```
CGPreflightScreenCaptureAccess = true
SCK stream started in 52.241167ms
first sample buffer 72.738042ms after start: status=Complete 3420x2224 stride=13696
  content_scale=1 scale_factor=2
seq=2 status=Complete display_time_ns=9940771198375 delta_ms=100.0 dirty_rects=3
  (0,0 3420x78) (3334,78 86x60) (424,138 2996x2086)
done: saved 5 image(s) out of 5 sample buffers; by status: {"Complete": 5}
```

`stride`(13696)は`width*4`(13680)より16バイト大きい。行ごとのパディングがあるので、
3M-1-bでエンコーダに渡すときは必ず`CVPixelBufferGetBytesPerRow`を使うこと。
`scale_factor=2`/`content_scale=1`で、dirtyRectsは**ポイント**単位(ピクセルではない)。

#### `SCStreamFrameInfo.displayTime`の罠

初回は最初のサンプルバッファでSIGTRAP(`EXC_BREAKPOINT`、`brk 1`)で落ちた。原因は
`.displayTime`を秒として扱っていたこと:

```swift
let displayTime = first[.displayTime] as? Double ?? 0   // ← 秒ではない
let displayTimeNs = UInt64(max(0, displayTime) * 1_000_000_000.0)  // ← Swiftがトラップ
```

`.displayTime`は**mach_absolute_timeの生ティック**。Apple silicon の timebase は 125/3
(24MHz)なので値は ~1.06e11 で、1e9倍すると 1.06e20 となりUInt64の範囲を超え、
`UInt64(Double)`が変換不能でトラップする。`mach_timebase_info`で換算する
(`machTicksToNanos`)。換算後は`delta_ms`が`--fps 10`に対してちょうど100.0になり、
値がuptimeと一致することで裏が取れる。

3W-1と同じ教訓の別形: **OSが返すメタデータの単位を推測しない**。

## このマシン固有のメモ

- zshの組み込み`log`が`/usr/bin/log`を隠す。統合ログは`/usr/bin/log show ...`で読む。
- Rustはmise経由。`cargo`は`~/.local/share/mise/shims`にある。
- tccdのログは`--info`レベルで十分に詳しい。TCC.dbはFull Disk Accessがないため読めない。
