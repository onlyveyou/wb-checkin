#!/bin/bash
# 网关统一入口：优先用 app 内置的 node，其次 WorkBuddy 自带 / 系统 node。
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
NODE=""

# 注意：不能只看 -x。从 app Resources 复制到家目录的 node 会带
# com.apple.provenance 标记，一执行就被 SIGKILL（137）。必须实际跑一次探测。
usable() { [ -x "$1" ] && "$1" -e "" >/dev/null 2>&1; }

# 1) app 内置（完全自包含的关键，路径跟随当前安装的网关目录）
usable "$DIR/node" && NODE="$DIR/node"

# 2) WorkBuddy 自带 node（版本号可能变，取最大的）
if [ -z "$NODE" ]; then
  CAND=$(ls -d "$HOME"/.workbuddy/binaries/node/versions/*/bin/node 2>/dev/null | sort -V | tail -1)
  [ -n "${CAND:-}" ] && usable "$CAND" && NODE="$CAND"
fi

# 3) 系统 / Homebrew
[ -z "$NODE" ] && command -v node >/dev/null 2>&1 && N="$(command -v node)" && usable "$N" && NODE="$N"
for c in /opt/homebrew/bin/node /usr/local/bin/node /usr/bin/node; do
  [ -z "$NODE" ] && usable "$c" && NODE="$c"
done

if [ -z "$NODE" ]; then
  echo "[run.sh] 找不到可用的 node，请重装 WB-Checkin（内置网关应带 node）" >&2
  exit 1
fi

exec "$NODE" "$DIR/gateway.mjs"
