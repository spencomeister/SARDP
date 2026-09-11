# dxgi-capture-poc (3W-1-a / 3W-1-b)

`sardp-stage3-os-integration-roadmap.md`のWindows 3W-1: 在席キャプチャ・エンコード基盤の
単体疎通確認。SARDP本体(`sardp`クレート)とは接続しない独立したCargoプロジェクト。

`sardp`クレートは`unsafe_code = "forbid"`をlintで強制しているが、DXGI/COM呼び出しは
本質的に`unsafe`を要するため、ここでは別プロジェクトとして切り出している。

## バイナリ

- **`dxgi-capture-poc`**(3W-1-a、`src/main.rs`): `IDXGIOutputDuplication`でデスクトップを
  毎秒数フレーム取得し、連番BMPとしてディスクへ保存(`captures/session_<unixtime>/`)。
  `DXGI_OUTDUPL_FRAME_INFO`(ダーティリージョン、ムーブリージョン、ポインタ位置等)をログ出力する。
- **`mf_h264_encode`**(3W-1-b、`src/bin/mf_h264_encode.rs`): 3W-1-aと同じキャプチャ結果
  (DXGIテクスチャ)を、CPUへ読み戻さずMedia Foundation Transform(MFT)経由の
  ハードウェアH.264エンコーダへ渡し、mp4ファイルへ書き出す
  (`captures/session_<unixtime>_h264/capture.mp4`)。実装の詳細と詰まった点は
  [MFT経由H.264エンコードの実装メモ](#mft経由h264エンコードの実装メモ)を参照。
- **`mf_probe`**(調査ツール、`src/bin/mf_probe.rs`): このマシンで使えるハードウェアH.264
  エンコーダMFTを列挙し、D3D11対応の有無・非同期かどうか・実際に対応する入力フォーマットを
  表示する。MFT絡みの問題が出た際の切り分けに再利用できるよう残してある。
- **`send_input_poc`**(3W-1-c、`src/bin/send_input_poc.rs`): `SendInput()`でキーボード・
  マウスの最小限の動作確認を行う。自前で起動したメモ帳ウィンドウの矩形内へマウスを
  移動・クリックし、`KEYEVENTF_UNICODE`でデモ文字列を打ち込む。フォーカス確認や
  文字間隔について踏んだ落とし穴は[SendInputの実装メモ](#sendinputの実装メモ)を参照。
- **`send_input_timing_probe`**(調査ツール、`src/bin/send_input_timing_probe.rs`):
  自前で作った古典的なWin32 EDITコントロールに対し、複数の文字間隔(0/1/5/15/50ms)で
  `SendInput`して読み返しが一致するかを確認する。`send_input_poc`が踏んだ文字化けが
  「Windows全般の制約」か「対象コントロール固有の問題」かの切り分けに使った。

## ビルド・実行

このマシンではMSVCツールチェーンの環境変数(`LIB`/`INCLUDE`)が既定のシェルにセットされて
いないため、リポジトリルートの[`tools/with-msvc.ps1`](../with-msvc.ps1)経由で実行する。

```powershell
powershell -File ..\with-msvc.ps1 . cargo build
powershell -File ..\with-msvc.ps1 . cargo run --bin dxgi-capture-poc
powershell -File ..\with-msvc.ps1 . cargo run --bin mf_h264_encode
```

### 既知の環境問題: VS "18" のMSVCツールセットが不完全

このマシンの `C:\Program Files\Microsoft Visual Studio\18\Community` は、MSVCツールセット
(`VC\Tools\MSVC\<version>`)に`include`ディレクトリが存在せず、`vcvarsall.bat`も欠落している。
そのため`vcvars64.bat`を呼んでも`vcvarsall.bat`が見つからずに失敗し、`msvcrt.lib`等の
インポートライブラリも見つからないままリンクすると`LNK1104`で失敗する。

代わりに`C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools`に完全な
MSVCツールセットが入っているため、`tools/with-msvc.ps1`はそちらの`vcvars64.bat`から
環境変数を読み込んでいる。3W-1-b/c以降もWindows APIをFFIで叩くたびに同じ回避策が
必要になる見込み。VS "18"側のインストールが修復された場合は、
`tools/with-msvc.ps1`内のパスを差し替えること。

## 出力

`captures/`はデスクトップの実画面を含むため`.gitignore`で除外している
(コミットしない)。動作確認が終わったら手動で削除すること。

## MFT経由H.264エンコードの実装メモ

3W-1-bは当初`IMFSinkWriter`の自動ハードウェア変換挿入
(`MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` + `MF_SINK_WRITER_D3D_MANAGER`)で組んだが、
`WriteSample`が`E_INVALIDARG`で失敗し続けた。`IMFSinkWriterEx::GetTransformForStream`で
確認すると内部的には正しくNVIDIAのD3D11対応MFTを選択できていたにもかかわらず失敗し、
原因は特定しきれなかった。ロードマップの「MFTへ直接渡す経路を組む」という記述どおり、
`IMFTransform`を自前でイベント駆動(`METransformNeedInput`/`METransformHaveOutput`)で
直接操作する構成に切り替えて解決した(現在の実装)。

その過程で判明した、この環境固有のMFT関連の落とし穴:

1. **入力フォーマットはNV12のみ**: `mf_probe`で調べたところ、このマシンのH.264ハードウェア
   エンコーダMFTはBGRA/ARGB32を直接受け付けず、NV12(および420O、YUY2等)のみ対応していた。
   そのためDXGI Desktop DuplicationのBGRAテクスチャを、`ID3D11VideoProcessor`でGPU上のまま
   NV12へ変換してからエンコーダへ渡している(CPU読み戻しなし)。
2. **非同期MFTはASYNC_UNLOCKが必須**: NVIDIAのMFTは非同期MFT(`MF_TRANSFORM_ASYNC=1`)で、
   `MF_TRANSFORM_ASYNC_UNLOCK`を立てるまで`SetOutputType`等の呼び出し自体を
   `MF_E_TRANSFORM_ASYNC_LOCKED`で拒否する。
3. **ブロッキング`GetEvent`が返ってこない**: `IMFMediaEventGenerator::GetEvent`を
   フラグ無し(ブロッキング)で呼ぶと、このMFTでは実際には返ってこず処理が停止した
   (`ProcessInput`前の`METransformNeedInput`待ち、ドレイン中の待ちの両方で確認)。
   `MF_EVENT_FLAG_NO_WAIT`でポーリングする形に変更して解決した。
4. **ドレイン完了は`METransformNeedInput`イベントで判定できるとは限らない**:
   `MFT_MESSAGE_COMMAND_DRAIN`後、出力し切ってもイベントが来ないことがあった。
   提出した入力サンプル数と出力サンプル数が一致した時点で完了とみなす方式に変更している
   (Bフレーム無しなので1入力=1出力の前提が成り立つ)。

`mf_probe`はNVIDIA H.264 Encoder MFT(D3D11対応・非同期)とMicrosoftのソフトウェア
H264 Encoder MFT(非D3D11対応)の2つを検出した。前者を使うことで、GPUのテクスチャを
そのままハードウェアエンコーダに渡す経路(DR-036が求める永続化パイプライン)が実現できている。

## SendInputの実装メモ

3W-1-cの初回実装で、実際に2つの問題を踏んだ(どちらも机上ではなく実行して発見したもの):

1. **`SetForegroundWindow`の戻り値`TRUE`は実際のフォーカス移動を保証しない**:
   これを信用してキー入力を送ったところ、意図したメモ帳ではなく別の前面ウィンドウへ
   入力が入ってしまう事故が起きた(スクリーンショットで確認)。修正として、
   `AttachThreadInput`で入力キューを結合した上で`SetForegroundWindow`を呼び、その後
   `GetForegroundWindow()`が実際に対象ウィンドウを指すまで確認してから初めてキー入力を
   送るようにした。確認できなければキー入力は一切送らない。
2. **文字列全体を1回の`SendInput`にまとめて送ると、モダン化されたメモ帳(WinUIベース)の
   テキストコントロールで文字化けする**(文字の脱落・直前の文字の繰り返し)。1文字ずつ
   個別に`SendInput`し、50ms/文字の待機を挟むことで解決した。

上記2番目の待機間隔について、「Windowsの入力キュー全般に必要な間隔」なのか
「検証対象(モダン化メモ帳)固有の問題」なのかを`send_input_timing_probe`で切り分けた。
結果: 自前で作った古典的なWin32 EDITコントロールへは**待機0msでも文字化けしない**。
一方モダン化メモ帳では15msでもまだ文字化けし、50msで初めて安定した。つまり安全な間隔は
相手のコントロール実装に依存し、単一の定数では一般化できない。SARDP本体(仕様2.12節
TextInput)で任意長の文章を打ち込む場合、相手アプリの種類を事前に知る手段がない以上、
固定の保守的な間隔を使うか、動的に調整する設計を検討する必要がある(`KNOWN_ISSUES.md`参照)。
