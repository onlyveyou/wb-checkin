#!/usr/bin/env bash
# 下载内置网关用的 node 二进制。
#
# 为什么不进仓库：单份 100MB+，超过 GitHub 单文件上限；而且 node 版本跟随上游，
# 没必要让仓库背着几百 MB 的历史。
#
# 用法：
#   scripts/fetch-node.sh          # macOS 通用包需要的两份（arm64 + x64）
#   scripts/fetch-node.sh --arm64  # 只下 Apple 芯片那份
#   scripts/fetch-node.sh --x64    # 只下 Intel 那份
#   scripts/fetch-node.sh --win    # Windows 的 node.exe（CI 里用的是同一地址）
#
# 通用包必须在 gateway-bundle/ 下同时有 node（arm64）和 node-x64，
# 安装时按机器架构挑一份解包成 node —— 少一份，那一类 Mac 就起不来网关。
set -euo pipefail

VERSION="v22.22.2"   # 与 .github/workflows/build-windows.yml 保持一致
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/gateway-bundle"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$DEST"
WANT=()
case "${1:-all}" in
  --arm64) WANT=(arm64) ;;
  --x64)   WANT=(x64) ;;
  --win)   WANT=(win) ;;
  all|"")  WANT=(arm64 x64) ;;
  *) echo "未知参数：$1（可用：--arm64 / --x64 / --win）" >&2; exit 2 ;;
esac

fetch() {  # fetch <url> <out>
  echo "下载 $1"
  if ! /usr/bin/curl -fL --retry 3 --max-time 600 -o "$2" "$1"; then
    echo "下载失败：$1" >&2
    echo "检查网络或代理；也可以手动下好放到 $(dirname "$2")" >&2
    exit 1
  fi
}

for arch in "${WANT[@]}"; do
  if [ "$arch" = "win" ]; then
    fetch "https://nodejs.org/dist/$VERSION/win-x64/node.exe" "$DEST/node.exe"
    continue
  fi
  if [ "$arch" = "arm64" ]; then
    pkg="node-$VERSION-darwin-arm64"
    out="$DEST/node"
  else
    pkg="node-$VERSION-darwin-x64"
    out="$DEST/node-x64"
  fi
  fetch "https://nodejs.org/dist/$VERSION/$pkg.tar.gz" "$TMP/$pkg.tar.gz"
  /usr/bin/tar -xzf "$TMP/$pkg.tar.gz" -C "$TMP"
  /bin/cp "$TMP/$pkg/bin/node" "$out"
  /bin/chmod 755 "$out"
  echo "已就位：$out（$(/usr/bin/lipo -archs "$out")，$("$out" --version)）"
done
