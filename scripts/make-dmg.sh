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
# 通用包（arm64 + Intel）走的是另一套 target 目录。默认优先通用包：
# 一份 dmg 两种 Mac 都能装，省得用户拿到自己机器跑不了的那份。
UNIVERSAL="$ROOT/src-tauri/target/universal-apple-darwin/release/bundle/macos/WB-Checkin.app"
if [ -n "${WB_CHECKIN_APP:-}" ]; then
  APP="$WB_CHECKIN_APP"
elif [ -d "$UNIVERSAL" ]; then
  APP="$UNIVERSAL"
else
  APP="$BUNDLE/macos/WB-Checkin.app"
fi

if [ ! -d "$APP" ]; then
  echo "找不到 $APP —— 先跑一次 tauri build（DMG 环节失败没关系，.app 已经在）" >&2
  exit 1
fi

VERSION="${1:-$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")}"
# 包名如实反映架构：通用包叫 universal，避免用户拿到 Intel 机器跑不了的 arm64 包
main_bin="$APP/Contents/MacOS/WB-Checkin"
ARCHS="$(/usr/bin/lipo -archs "$main_bin" 2>/dev/null || echo unknown)"
case "$ARCHS" in
  *arm64*x86_64*|*x86_64*arm64*) ARCH_TAG=universal ;;
  *arm64*)                       ARCH_TAG=arm64 ;;
  *x86_64*)                      ARCH_TAG=x64 ;;
  *)                             ARCH_TAG=unknown ;;
esac
OUT="$BUNDLE/dmg/WB-Checkin_${VERSION}_${ARCH_TAG}.dmg"

# 上一次失败的打包可能留下挂载卷和临时读写镜像，先清干净再开工
VOLS="$(/usr/bin/hdiutil info | /usr/bin/grep -o '/Volumes/dmg\.[A-Za-z0-9]*' | sort -u || true)"
for vol in $VOLS; do
  /usr/bin/hdiutil detach "$vol" -force >/dev/null 2>&1 || true
done
rm -f "$BUNDLE"/macos/rw.*.dmg

# hdiutil 默认把临时镜像写到 /var/folders（系统盘）。系统盘剩几 GB 时它会直接报
# 「设备上无剩余空间」，而仓库所在的盘可能还有几百 GB。先把它改到仓库旁边的 .dmg-tmp。
TMPDIR="${WB_CHECKIN_TMPDIR:-$ROOT/.dmg-tmp}"
mkdir -p "$TMPDIR"
export TMPDIR

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
# 用 ditto 而不是 cp -R：签名和扩展属性都得原样带过去，否则用户那边会报「已损坏」
/usr/bin/ditto "$APP" "$STAGE/WB-Checkin.app"
ln -s /Applications "$STAGE/Applications"

# 没有 Apple 开发者签名，下载后首次双击一定被 Gatekeeper 拦。
# 说明放在 dmg 里（而不是只写在 README）：用户打开 dmg 时就在眼前，不用先去翻文档。
cat > "$STAGE/首次打开必看.txt" <<'TXT'
WB-Checkin 首次打开说明
=======================

双击提示「无法打开，因为 Apple 无法验证此 App」或「已损坏」时，按下面任一种处理：

方法一（推荐）
  在访达里找到 WB-Checkin，按住 Control 键点它，选「打开」，再点一次「打开」。

方法二
  打开「系统设置 → 隐私与安全性」，把页面往下拉，找到 WB-Checkin 的提示，
  点「仍要打开」，然后回访达双击。

方法三（前两种都不行时）
  打开「终端」，粘贴这一行回车：
  sudo xattr -dr com.apple.quarantine /Applications/WB-Checkin.app

为什么会被拦：这个工具没有上架 App Store，也没有 Apple 开发者签名，
所以系统默认不信任它。程序不联网上传任何数据，账号、配置、日志都在本机。
TXT

mkdir -p "$BUNDLE/dmg"
rm -f "$OUT"
/usr/bin/hdiutil create -volname "WB-Checkin" -srcfolder "$STAGE" -ov -format UDZO "$OUT" >/dev/null

# 每次打包只留新包：本地产物目录和交付目录里的旧 dmg / dmg.zip 一律清掉。
# 放在成功之后做，打包失败时旧包还在，不会把能用的包先删了。
find "$BUNDLE/dmg" -maxdepth 1 -type f \
  \( -name 'WB-Checkin_*.dmg' -o -name 'WB-Checkin_*.dmg.zip' \) \
  ! -name "$(basename "$OUT")" -exec /bin/rm -f {} +

DELIVER_DIR="${WB_CHECKIN_DMG_DIR:-/Volumes/Apreslavie/wb-checkin}"
if [ -d "$DELIVER_DIR" ]; then
  NEW_NAME="WB-Checkin-${VERSION}-${ARCH_TAG}.dmg"
  find "$DELIVER_DIR" -maxdepth 1 -type f \
    \( -name 'WB-Checkin-*.dmg' -o -name 'WB-Checkin-*.dmg.zip' \) \
    ! -name "$NEW_NAME" -exec /bin/rm -f {} +
  /bin/cp -f "$OUT" "$DELIVER_DIR/$NEW_NAME"
  echo "已交付：${DELIVER_DIR}/${NEW_NAME}（旧包已清理）"
fi

echo "$OUT"
