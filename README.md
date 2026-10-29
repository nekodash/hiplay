# hifi-player

個人用の音質優先・音楽再生専用プレイヤー（macOS / Windows）。

## 音質のための方針

- 出力デバイスを専有（macOS: Hog Mode / Windows: WASAPI 排他モード）し、OS のミキサーを通さない
- 曲ごとにデバイスのサンプルレートを切り替え、リサンプリングしない
- 物理フォーマットはそのレートで最大のビット深度を選ぶ
- ソフトウェア音量は持たない（音量は DAC・アンプ側で調整）
- 同じフォーマットが続く間はストリームを止めずにギャップレス再生

## 構成

- `engine/` 再生エンジン（Symphonia でデコード → リングバッファ → OS ごとの出力）
- `cli/` 動作確認用 CLI `hiplay`

## 使い方（CLI）

```bash
cargo build --release
./target/release/hiplay devices
./target/release/hiplay -d "デバイス名の一部" ~/Music/アルバム
```

再生中は `p`（一時停止）、`n`（次）、`b`（前）、`s 秒数`（シーク）、`q`（終了）を入力して Enter。

## 対応状況

- [x] FLAC / ALAC / WAV / AIFF / MP3 / AAC
- [x] macOS 出力（Hog Mode・レート切替・物理フォーマット選択）
- [ ] Windows 出力（WASAPI 排他モード）
- [ ] GUI（フォルダ再生 + キュー）
- [ ] 非対応レートの音源をデバイスに合わせて変換する処理
