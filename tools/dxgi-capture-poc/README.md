# dxgi-capture-poc (3W-1-a)

`sardp-stage3-os-integration-roadmap.md`の3W-1-a: DXGI Desktop Duplication単体疎通確認。
SARDP本体(`sardp`クレート)とは接続しない独立したCargoプロジェクト。

`sardp`クレートは`unsafe_code = "forbid"`をlintで強制しているが、DXGI/COM呼び出しは
本質的に`unsafe`を要するため、ここでは別プロジェクトとして切り出している。

## やること

- `IDXGIOutputDuplication`でデスクトップを毎秒数フレーム取得
- 取得したテクスチャを連番BMPとしてディスクへ保存(`captures/session_<unixtime>/`)
- `DXGI_OUTDUPL_FRAME_INFO`(ダーティリージョン、ムーブリージョン、ポインタ位置等)をログ出力

## ビルド・実行

このマシンではMSVCツールチェーンの環境変数(`LIB`/`INCLUDE`)が既定のシェルにセットされて
いないため、リポジトリルートの[`tools/with-msvc.ps1`](../with-msvc.ps1)経由で実行する。

```powershell
powershell -File ..\with-msvc.ps1 . cargo build
powershell -File ..\with-msvc.ps1 . cargo run
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
