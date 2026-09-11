# docs/ — SARDP 設計文書の索引

Secure Adaptive Remote Desktop Protocol(SARDP)の仕様・設計経緯・実装計画をまとめたディレクトリです。実装は `src/`(コアクレート `sardp`)、`sardp-win/`(Windows OS統合)、`tools/`(検証用PoC)にあり、既知の課題は リポジトリ直下の `KNOWN_ISSUES.md` に集約しています。

## 読む順番

1. **[SARDP_NormativeSpec.md](SARDP_NormativeSpec.md) — 規範仕様 v0.3(実装の拠り所)**
   実装者向けの現行仕様。MUST / SHOULD / MAY は RFC 2119 の意味。構成は次のとおりです。
   - Part 1 Protocol Overview / Part 2 Normative Wire Specification(Envelope、StreamPrologue、各メッセージ: 2.10 映像、2.12 入力、2.13 音声、2.14 TransportFeedback など)
   - Part 3 Security Model / Part 4 State Machines(Connection、Stream Lifecycle、VideoStream の Channel/Instance 2層、Input(IMEモード・押下不変条件)、Permission、Reconnection、タイムアウト表、エラーハンドリングマトリクス)
   - Part 5 Transport Bindings / Part 6 Media Model(エンコーダTier区分)/ Part 7 OS Integration / Part 8 Implementation Requirements
   - Appendix A Decision Records(DR-xxx 索引。本文中の DR 参照はここに対応)/ Appendix B 棄却案・将来課題
   仕様変更は必ずこのファイルに反映し、DR を追記してから実装します。

2. **[SARDP_PoC_Brief_for_ClaudeCode.md](SARDP_PoC_Brief_for_ClaudeCode.md) — PoC 実装ブリーフ(スコープ定義)**
   規範仕様を実装に落とすための「何を検証したいか / どこを省略してよいか」の取り決め。M1〜M6 のマイルストーン、`ffmpeg` によるタイムコード埋め込み合成フレームなど、PoC 固有の割り切りはここが根拠です。仕様書そのものではありません。

3. **[sardp-stage3-os-integration-roadmap.md](sardp-stage3-os-integration-roadmap.md) — Stage 3(実OS統合)ロードマップ**
   Windows → macOS → Debian(GNOME)の順で、実キャプチャ・ハードウェアエンコード・入力注入・無人アクセスを段階的に進める計画と受け入れ基準。3W-1(Windows 在席)から着手し、各サブマイルストーン完了ごとに立ち止まって報告する運用です。3W-1 の実装は `sardp-win/` と `tools/dxgi-capture-poc/` に対応します。

4. **[SARDP_MessageSchema.md](SARDP_MessageSchema.md) — メッセージスキーマ v0.1(設計経緯つき)**
   ワイヤフォーマットの草案と、採用案・棄却案とその理由の記録(設計史ジャーナル)。現行の定義は規範仕様 v0.3 が優先します。「なぜこの形になったか」を知りたいときにだけ参照してください。

## 文書間の対応関係

| 知りたいこと | 参照先 |
|---|---|
| メッセージの形式・状態機械・タイムアウト値 | 規範仕様 Part 2 / Part 4(4.7 タイムアウト表) |
| ある設計判断の理由 | 規範仕様 Appendix A(DR-xxx)→ 経緯はメッセージスキーマ |
| PoC でどこまで実装するか、何を省略したか | PoC ブリーフ |
| 実OS(Windows/macOS/Linux)対応の順序と制約 | Stage 3 ロードマップ、規範仕様 Part 7 |
| 実装で判明した環境依存の問題・未対応事項 | `../KNOWN_ISSUES.md` |
| PoC ツール(DXGI / MFT / SendInput)の使い方と落とし穴 | `../tools/dxgi-capture-poc/README.md` |

## 表記

- 文書は日本語、コード中のコメント・識別子は英語です。
- 規範仕様の節番号(例: 2.10、4.4.2)と DR 番号(例: DR-029)は、コードコメント・コミットメッセージ・`KNOWN_ISSUES.md` から相互参照する際の共通キーです。
