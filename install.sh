#!/usr/bin/env bash
# WB-Checkin 安装脚本：从最新 Release 取 dmg，装进 /Applications，并清掉隔离属性。
#
#   curl -fsSL https://raw.githubusercontent.com/onlyveyou/wb-checkin/main/install.sh | bash
#
# 为什么需要这个脚本：浏览器 / AirDrop / 微信 / 邮件都会给下载来的文件打上
# com.apple.quarantine，而 curl 不会。本包没有 Apple 开发者签名，一旦带上隔离
# 属性，Gatekeeper 的静态校验就会判定「已损坏」——由终端下载安装可以完全绕开这一步。
set -euo pipefail

REPO="onlyveyou/wb-checkin"
APP_NAME="WB-Checkin"
DEST="/Applications/${APP_NAME}.app"

say() { printf '%s\n' "$*"; }
warn() { printf '！%s\n' "$*" >&2; }
die() { printf '错误：%s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "只能在 macOS 上运行。"

ARCH="$(uname -m)"
if [ "$ARCH" != "arm64" ]; then
  die "当前 Release 只提供 arm64（Apple 芯片）安装包，本机是 ${ARCH}。
       Intel 机器请自行构建（产物无隔离属性，也不需要签名）：
         git clone https://github.com/${REPO}.git && cd wb-checkin
         npm install && npm run app:build -- --target universal-apple-darwin"
fi

command -v curl >/dev/null 2>&1 || die "找不到 curl。"
[ -w /Applications ] || die "/Applications 不可写，请用有管理员权限的账户重新执行。"

TMP=""
MNT=""
cleanup() {
  if [ -n "$MNT" ] && [ -d "$MNT" ]; then
    /usr/bin/hdiutil detach "$MNT" -force >/dev/null 2>&1 || true
  fi
  if [ -n "$TMP" ] && [ -d "$TMP" ]; then
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

say "→ 查询最新版本…"
API="https://api.github.com/repos/${REPO}/releases/latest"
JSON="$(curl -fsSL "$API")" || die "拉取 Release 信息失败（网络不通或被 GitHub 限流）。
       可稍等重试，或到 https://github.com/${REPO}/releases/latest 手动下载 dmg 后执行
       sudo xattr -cr /Applications/${APP_NAME}.app"

TAG="$(printf '%s' "$JSON" | /usr/bin/sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
URL="$(printf '%s' "$JSON" \
  | /usr/bin/grep -o '"browser_download_url":[[:space:]]*"[^"]*\.dmg"' \
  | head -n 1 \
  | /usr/bin/sed 's/.*"\(https[^"]*\)".*/\1/')"
[ -n "$URL" ] || die "最新 Release 里没有 dmg 产物，请检查 https://github.com/${REPO}/releases"

TMP="$(mktemp -d)"
DMG="${TMP}/$(basename "$URL")"

say "→ ${TAG:-latest}：下载 $(basename "$URL")"
curl -fL --progress-bar "$URL" -o "$DMG" || die "下载失败，请检查网络后重试。"

say "→ 挂载 dmg…"
MNT="$(/usr/bin/hdiutil attach "$DMG" -nobrowse -readonly \
  | /usr/bin/grep -o '/Volumes/.*' | /usr/bin/tail -n 1 | /usr/bin/sed 's/[[:space:]]*$//')"
[ -n "$MNT" ] || die "挂载 dmg 失败。"

SRC="${MNT}/${APP_NAME}.app"
[ -d "$SRC" ] || die "dmg 里没有 ${APP_NAME}.app。"

# 覆盖安装前必须先退出，否则正在运行的进程会占住可执行文件
if pgrep -f "${APP_NAME}.app/Contents/MacOS" >/dev/null 2>&1; then
  say "→ 退出正在运行的 ${APP_NAME}…"
  pkill -f "${APP_NAME}.app/Contents/MacOS" >/dev/null 2>&1 || true
  sleep 1
fi

say "→ 安装到 ${DEST}"
if [ -d "$DEST" ]; then
  rm -rf "$DEST"
fi
# 用 ditto 而不是 cp -R：签名、扩展属性、资源分支都要原样带过去
/usr/bin/ditto "$SRC" "$DEST" || die "拷贝到 /Applications 失败。"

# 终端下载本不该带隔离属性，仍清一遍：dmg 也可能是别处转存来的
/usr/bin/xattr -cr "$DEST" 2>/dev/null || true

if /usr/bin/codesign --verify --strict "$DEST" >/dev/null 2>&1; then
  say "→ 签名结构校验通过（ad-hoc，非 Developer ID）"
else
  warn "签名结构校验未通过，可能仍会报「已损坏」；见 README「没有签名意味着什么」一节"
fi

say ""
say "装好了：${DEST}"
say "启动：双击图标，或执行  open -a ${APP_NAME}"
say "建议把它加进「系统设置 → 通用 → 登录项」——进程没在跑的时候，兜底签到不会触发。"
