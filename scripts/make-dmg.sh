#!/usr/bin/env bash
# 从已构建的 WB-Checkin.app 直接造 DMG。
#
# 为什么不用 `tauri build --bundles dmg` 收尾：它内部调用 create-dmg 派生的
# bundle_dmg.sh，环节里依赖 Finder 自动化。在受限环境（自动化被拒 -10004、
# 无头会话、CI）里那一步会失败，还会留下一个没卸载的挂载卷，把整个打包打断。
# 此时 .app 本体其实已经构建并签名好了 —— DMG 只是个外壳，用 hdiutil 直接包即可。
#
# 用法：scripts/make-dmg.sh [版本号]
#       版本号省略时取 .app 里的 CFBundleShortVersionString
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE="$ROOT/src-tauri/target/release/bundle"
APP="$BUNDLE/macos/WB-Checkin.app"

if [ ! -d "$APP" ]; then
  echo "找不到 $APP —— 先跑一次 tauri build（DMG 环节失败没关系，.app 已经在）" >&2
  exit 1
fi

VERSION="${1:-$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")}"
OUT="$BUNDLE/dmg/WB-Checkin_${VERSION}_aarch64.dmg"

# 上一次失败的打包可能留下挂载卷和临时读写镜像，先清干净再开工
VOLS="$(/usr/bin/hdiutil info | /usr/bin/grep -o '/Volumes/dmg\.[A-Za-z0-9]*' | sort -u || true)"
for vol in $VOLS; do
  /usr/bin/hdiutil detach "$vol" -force >/dev/null 2>&1 || true
done
rm -f "$BUNDLE"/macos/rw.*.dmg

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
# 用 ditto 而不是 cp -R：签名和扩展属性都得原样带过去，否则用户那边会报「已损坏」
/usr/bin/ditto "$APP" "$STAGE/WB-Checkin.app"
ln -s /Applications "$STAGE/Applications"

mkdir -p "$BUNDLE/dmg"
rm -f "$OUT"
/usr/bin/hdiutil create -volname "WB-Checkin" -srcfolder "$STAGE" -ov -format UDZO "$OUT" >/dev/null

echo "$OUT"
