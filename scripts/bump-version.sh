#!/usr/bin/env bash
# 升版本号的唯一入口：一次同步四处，并在 CHANGELOG 顶部插入待填条目。
#
# 为什么要有这个脚本：版本号散在四处 —— package.json、src-tauri/tauri.conf.json、
# src-tauri/Cargo.toml、src-tauri/Cargo.lock。手工 sed 漏掉一处，就会出现
# 「app 里显示 0.10.2、Release 叫 0.10.3」这类错位，而且只在装完才看得出来。
#
# 用法：
#   scripts/bump-version.sh 0.11.0        # 改版本号 + 插 CHANGELOG 条目
#   scripts/bump-version.sh 0.11.0 --dry-run   # 只打印将要改什么，不落盘
#
# 不改的东西：tag、Release、构建产物。tag 由发布流程单独打（见 README「开发」）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PKG="package.json"
CONF="src-tauri/tauri.conf.json"
TOML="src-tauri/Cargo.toml"
LOCK="src-tauri/Cargo.lock"
CHANGELOG="CHANGELOG.md"

DRY_RUN=0
NEW=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help) sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "未知选项：$arg" >&2; exit 2 ;;
    *) NEW="$arg" ;;
  esac
done

[ -n "$NEW" ] || { echo "用法：scripts/bump-version.sh <x.y.z> [--dry-run]" >&2; exit 2; }

# 只接受严格的三段数字。带 v 前缀、带 -beta 后缀一律拒绝 —— 版本号要能被
# 四处解析器（npm / Tauri / Cargo）同时接受，含糊的写法迟早有一处读错。
if ! printf '%s' "$NEW" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "版本号必须是 x.y.z 形式（例如 0.11.0），收到：$NEW" >&2
  exit 2
fi

for f in "$PKG" "$CONF" "$TOML" "$LOCK"; do
  [ -f "$f" ] || { echo "找不到 $f，请在仓库根执行" >&2; exit 1; }
done

CUR="$(grep -m1 '"version"' "$PKG" | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
[ -n "$CUR" ] || { echo "读不出现有版本号（${PKG}）" >&2; exit 1; }

if [ "$NEW" = "$CUR" ]; then
  echo "版本号已经是 ${CUR}，无需改动。" >&2
  exit 1
fi

# 新版本必须比现有版本大。降版本号几乎总是笔误，允许多一步手改文件，不在这里放行。
if ! printf '%s\n%s\n' "$CUR" "$NEW" | sort -V -C; then
  echo "新版本 $NEW 不大于现有 ${CUR}。若确实要降版，请直接改文件。" >&2
  exit 1
fi

TODAY="$(date +%Y-%m-%d)"

if grep -q "^## \[${NEW}\]" "$CHANGELOG" 2>/dev/null; then
  echo "$CHANGELOG 里已经有 $NEW 的条目，先确认是不是重复升版。" >&2
  exit 1
fi

echo "版本：$CUR → ${NEW}（${TODAY}）"
[ "$DRY_RUN" -eq 1 ] && echo "（dry-run，不写文件）"

# 先把四个文件备份到临时目录，任何一步失败整体恢复 —— 半改状态比不改更糟：
# 下次构建会用新版本号打包，却仍然带着旧的 Cargo.lock 版本，产物自相矛盾。
BACKUP="$(mktemp -d)"
restore() {
  set +e
  for f in "$PKG" "$CONF" "$TOML" "$LOCK" "$CHANGELOG"; do
    if [ -f "$BACKUP/$(basename "$f")" ]; then
      cp "$BACKUP/$(basename "$f")" "$f"
    fi
  done
  echo "已回滚到改动前的状态。" >&2
}
trap restore ERR
for f in "$PKG" "$CONF" "$TOML" "$LOCK" "$CHANGELOG"; do
  cp "$f" "$BACKUP/$(basename "$f")"
done

if ! python3 - "$PKG" "$CONF" "$TOML" "$LOCK" "$NEW" "$TODAY" "$CHANGELOG" "$DRY_RUN" <<'PY'
import json
import re
import sys

pkg, conf, toml_path, lock_path, new, today, changelog, dry = sys.argv[1:9]
dry = dry == "1"

def write_text(path, text):
    if dry:
        return
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(text)

def read_text(path):
    with open(path, encoding="utf-8") as fh:
        return fh.read()

# 1. package.json / tauri.conf.json：顶层 "version" 字段。
#    用 JSON 解析而不是正则，避免误伤 dependencies 里的版本范围。
for path in (pkg, conf):
    data = json.loads(read_text(path))
    old = data.get("version")
    data["version"] = new
    text = json.dumps(data, ensure_ascii=False, indent=2) + "\n"
    write_text(path, text)
    print(f"  {path}: {old} → {new}")

# 2. Cargo.toml：只改 [package] 段里的第一处 version，不碰 [dependencies] 里的。
text = read_text(toml_path)
lines = text.splitlines(keepends=True)
in_package = False
done = False
for i, line in enumerate(lines):
    stripped = line.strip()
    if stripped.startswith("["):
        in_package = stripped == "[package]"
        continue
    if in_package and not done and re.match(r"^version\s*=", stripped):
        old = stripped.split("=", 1)[1].strip().strip('"')
        lines[i] = f'version = "{new}"\n'
        done = True
        print(f"  {toml_path}: {old} → {new}（[package] 段）")
if not done:
    sys.exit("Cargo.toml 里没找到 [package] 段的 version 行")
write_text(toml_path, "".join(lines))

# 3. Cargo.lock：改本包自己的那条（name = "wb-checkin" 的下一行），
#    别按行号或全局替换 —— 锁文件里全是 name/version 对。
text = read_text(lock_path)
lines = text.splitlines(keepends=True)
done = False
for i, line in enumerate(lines):
    if line.strip() == 'name = "wb-checkin"':
        if i + 1 < len(lines) and lines[i + 1].strip().startswith("version"):
            old = lines[i + 1].strip().split("=", 1)[1].strip().strip('"')
            lines[i + 1] = f'version = "{new}"\n'
            done = True
            print(f"  {lock_path}: {old} → {new}")
            break
if not done:
    sys.exit('Cargo.lock 里没找到 name = "wb-checkin" 条目')
write_text(lock_path, "".join(lines))

# 4. CHANGELOG：在 [未发布] 之后插入新版本骨架。
text = read_text(changelog)
marker = "## [未发布]"
if marker not in text:
    sys.exit(f"{changelog} 里找不到「{marker}」标记")
entry = (
    f"## [{new}] - {today}\n\n"
    "### 新增\n\n"
    "### 变更\n\n"
    "### 修复\n\n"
)
text = text.replace(marker, marker + "\n\n" + entry.rstrip("\n"), 1)
write_text(changelog, text)
print(f"  {changelog}: 插入 [{new}] - {today} 条目（三节待填）")
PY
then
  restore
  exit 1
fi

# 自检：四处读回来必须一致。dry-run 时文件没动，按预期版本校验。
check() {
  local v
  v=$(grep -m1 '"version"' "$PKG" | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')
  [ "$v" = "$NEW" ] || { echo "自检失败：$PKG 是 $v" >&2; return 1; }
  v=$(grep -m1 '"version"' "$CONF" | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')
  [ "$v" = "$NEW" ] || { echo "自检失败：$CONF 是 $v" >&2; return 1; }
  v=$(awk '/^\[package\]/{f=1;next} /^\[/{f=0} f&&/^version *=/{split($0,a,"\""); print a[2]; exit}' "$TOML")
  [ "$v" = "$NEW" ] || { echo "自检失败：$TOML 是 $v" >&2; return 1; }
  v=$(awk '/^name = "wb-checkin"$/{getline; if ($1=="version"){gsub(/"/,"",$3); print $3; exit}}' "$LOCK")
  [ "$v" = "$NEW" ] || { echo "自检失败：$LOCK 是 $v" >&2; return 1; }
}

if [ "$DRY_RUN" -eq 1 ]; then
  echo "dry-run 结束，未改动任何文件。"
  exit 0
fi

check || { restore; exit 1; }
trap - ERR

echo
echo "四处版本号已一致为 ${NEW}。"
echo "下一步：填 CHANGELOG 的 $NEW 条目 → 构建 → 提交 → 打 tag v${NEW}（tag 会触发 Windows 出包）"
