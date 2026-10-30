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
- `app/` GUI（Tauri 2）。画面は `app/ui/` の素の HTML/JS で、再生はエンジンに任せる
  - `app/dev/mock.html` は模擬バックエンドで画面だけをブラウザで確かめるためのページ

## GUI のビルド

```bash
cd app && npm install && npx tauri build --bundles app
```

`target/release/bundle/macos/HiFi Player.app` ができる。開発中は `cd app && npx tauri dev`。

操作: 曲をダブルクリックで再生、Space で再生／一時停止、←→ で 10 秒移動、Shift+←→ で前後の曲。

## 使い方（CLI）

```bash
cargo build --release
./target/release/hiplay devices
./target/release/hiplay -d "デバイス名の一部" ~/Music/アルバム
```

再生中は `p`（一時停止）、`n`（次）、`b`（前）、`s 秒数`（シーク）、`q`（終了）を入力して Enter。

## GitHub Actions

`main` への push と PR で macOS（ユニバーサル版の dmg）と Windows（NSIS インストーラ）をビルドし、
Actions の実行結果の Artifacts に置く。`v0.1.0` のようなタグを push すると、両方を添付したリリースの下書きを作る。

```bash
git tag v0.1.0 && git push origin v0.1.0
```

署名・公証はしていない（macOS はアドホック署名のみ）。ダウンロードした dmg のアプリは Gatekeeper に止められるので、
初回は Finder で右クリック →「開く」とするか、次のコマンドで隔離属性を外す。

```bash
xattr -dr com.apple.quarantine "/Applications/HiFi Player.app"
```

Windows の初回起動では SmartScreen の「詳細情報」→「実行」で起動する。

## 対応状況

- [x] FLAC / ALAC / WAV / AIFF / MP3 / AAC
- [x] macOS 出力（Hog Mode・レート切替・物理フォーマット選択）
- [x] Windows 出力（WASAPI 排他モード・ポーリング駆動。実機未検証）
- [x] 再生中の切断（一時停止して位置を保持、同じデバイスにつなぎ直して再開。既定デバイスへは切り替えない）
- [x] GUI（フォルダ再生 + キュー、ドラッグ＆ドロップ、信号経路の表示、出力デバイスの切り替え）
- [ ] 非対応レートの音源をデバイスに合わせて変換する処理
