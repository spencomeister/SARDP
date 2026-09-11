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
