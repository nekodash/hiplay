#!/bin/bash
# HiPlay を GitHub のリリースから /Applications に入れる。
#
#   curl -fsSL https://raw.githubusercontent.com/nekodash/hiplay/main/scripts/install-macos.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/nekodash/hiplay/main/scripts/install-macos.sh | bash -s v0.2.0
#
# ブラウザで落とした dmg には隔離属性が付き、未公証のアプリは Gatekeeper に止められる。
# curl で落とせばこの属性が付かないので、「このまま開く」の操作なしで起動できる。
# 更新するときも同じコマンドを実行すればよい。
#
# 環境変数（主に確認用）:
#   HIPLAY_DMG_URL  リリースを探さず、この URL（file:// も可）の dmg を使う
#   HIPLAY_DEST     入れる先（既定は /Applications）

set -euo pipefail

REPO="nekodash/hiplay"
TAG="${1:-latest}"
DEST="${HIPLAY_DEST:-/Applications}"

url="${HIPLAY_DMG_URL:-}"
if [ -z "$url" ]; then
  if [ "$TAG" = "latest" ]; then
    api="https://api.github.com/repos/$REPO/releases/latest"
  else
    api="https://api.github.com/repos/$REPO/releases/tags/$TAG"
  fi
  url=$(curl -fsSL "$api" | grep -o '"browser_download_url": *"[^"]*\.dmg"' | head -1 | sed -E 's/.*"(https[^"]+)"$/\1/') || true
  if [ -z "$url" ]; then
    echo "リリース（$TAG）に dmg が見つかりません。リリースが公開済みか確認してください。" >&2
    exit 1
  fi
fi

tmp=$(mktemp -d)
cleanup() {
  hdiutil detach -quiet "$tmp/mnt" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

echo "ダウンロード: $url"
curl -fL --progress-bar -o "$tmp/HiPlay.dmg" "$url"

hdiutil attach -quiet -nobrowse -readonly -mountpoint "$tmp/mnt" "$tmp/HiPlay.dmg"
app=$(find "$tmp/mnt" -maxdepth 1 -name "*.app" | head -1)
if [ -z "$app" ]; then
  echo "dmg の中にアプリがありません" >&2
  exit 1
fi
name=$(basename "$app")

# 起動中なら終了させてから置き換える
osascript -e "quit app \"${name%.app}\"" 2>/dev/null || true

rm -rf "$DEST/$name"
ditto "$app" "$DEST/$name"
# 念のため（curl では付かないが、手元の dmg を使った場合に備える）
xattr -dr com.apple.quarantine "$DEST/$name" 2>/dev/null || true

echo "インストールしました: $DEST/$name"
