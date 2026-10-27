//! 分层快照：把「一个账号在客户端里的完整现场」存下来，切换时整体换回去。
//!
//! 只写登录态文件（L1）会让客户端重启后进入一个新账号，但会话目录还是上一个
//! 账号的 Cookie、侧边栏仍指向旧 uid —— 表现就是「切号后会话没了」。这里把
//! 四层一起快照，切换时成套恢复：
//!
//! | 层 | 内容 | 路径 |
//! |----|------|------|
//! | L1 | 官方登录态文件 | `~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info` |
//! | L2 | 账号级用户目录 | `~/.workbuddy/storage/user-<uid>` 与 `user-<uid>-personal` |
//! | L3 | Chromium 会话 | `~/.workbuddy/app/session/`（跳过纯缓存目录） |
//! | L4 | 侧边栏与当前会话路由 | `~/.workbuddy/<uid>/sidebar-list-snapshot.json`、`startup-route.json` |
//!
//! 会话正文（`~/.workbuddy/projects/**/*.jsonl`）不按账号分目录，切号后原地还在，
//! 因此不需要复制；L4 恢复之后新账号的侧边栏就能看到并打开原来那条会话。
//!
//! 复制用 `cp -cR`（APFS 写时复制）：600 MB 级别的 session 目录也是秒级、几乎不占
//! 额外磁盘。`cp -c` 不受支持时（非 APFS 卷）自动退回普通复制。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths::{data_dir, now_ms};
use crate::switch;

/// 纯缓存目录：没有状态价值，占体积的大头，快照时跳过。
const SESSION_EXCLUDES: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "ShaderCache",
    "GrShaderCache",
    "VideoDecodeStats",
    "blob_storage",
    "component_crx_cache",
    "Crashpad",
];

/// 客户端重建这些目录即可，不需要从快照里恢复（恢复前也一律清掉）。
fn wb_root() -> PathBuf {
    crate::paths::home_dir().join(".workbuddy")
}

pub fn session_dir() -> PathBuf {
    wb_root().join("app").join("session")
}

fn storage_dir() -> PathBuf {
    wb_root().join("storage")
}

/// 每个账号自己的界面状态目录（`~/.workbuddy/<uid>/`）。
fn account_dir(uid: &str) -> PathBuf {
    wb_root().join(uid)
}

/// 快照根目录：与其他本地数据放在一起，权限沿用 `~/.wb-checkin` 的 0700。
pub fn profiles_root() -> PathBuf {
    data_dir().join("profiles")
}

/// uid 来自账号库/official 文件，落地成目录名前先收敛字符集，避免路径穿越。
fn safe_name(uid: &str) -> String {
    let cleaned: String = uid
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

fn snapshot_dir(uid: &str) -> PathBuf {
    profiles_root().join(safe_name(uid))
}

fn run(program: &str, args: &[String]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn remove_path(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

/// 复制一个目录/文件。优先 APFS clonefile，失败退回普通递归复制。
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    if !from.exists() {
        return Ok(());
    }
    if to.exists() {
        remove_path(to);
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建目录失败：{error}"))?;
    }

    let source = from.to_string_lossy().to_string();
    let target = to.to_string_lossy().to_string();
    if run("cp", &["-cR".to_string(), source.clone(), target.clone()]) {
        return Ok(());
    }
    if run("cp", &["-R".to_string(), source, target]) {
        return Ok(());
    }
    Err(format!("复制失败：{}", from.to_string_lossy()))
}

fn is_excluded(name: &str) -> bool {
    SESSION_EXCLUDES.contains(&name)
}

/// 存 L3：只复制非缓存项，逐项克隆。
fn save_session(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|error| format!("创建快照目录失败：{error}"))?;
    let entries = std::fs::read_dir(from).map_err(|error| format!("读取会话目录失败：{error}"))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if is_excluded(&name) {
            continue;
        }
        copy_tree(&entry.path(), &to.join(&name))?;
    }
    Ok(())
}

/// 恢复 L3：先清掉现有会话目录里所有非缓存项，再整份拷回快照内容。
///
/// 不做增量合并：两个账号的 Local Storage 键空间可能重叠，合并会留下两边都不认的
/// 脏状态。
fn restore_session(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|error| format!("创建会话目录失败：{error}"))?;
    if let Ok(entries) = std::fs::read_dir(to) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_excluded(&name) {
                continue;
            }
            remove_path(&entry.path());
        }
    }
    let entries = std::fs::read_dir(from).map_err(|error| format!("读取快照失败：{error}"))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        copy_tree(&entry.path(), &to.join(&name))?;
    }
    Ok(())
}

fn dir_size_kb(path: &Path) -> u64 {
    let output = Command::new("du")
        .args(["-sk", &path.to_string_lossy()])
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0),
        _ => 0,
    }
}

/// 账号级用户目录（L2）的两个后缀：普通与 personal。
const ACCOUNT_SUFFIXES: [&str; 2] = ["", "-personal"];

/// 保存一个账号的完整现场。调用前必须已确认客户端退出，否则 Cookie 可能只写了一半。
pub fn save(uid: &str, label: &str) -> Result<Value, String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Err("缺少账号 uid，无法定位该账号的本地目录".to_string());
    }

    let root = snapshot_dir(uid);
    std::fs::create_dir_all(&root).map_err(|error| format!("创建快照目录失败：{error}"))?;

    // L1：登录态文件（客户端未登录过时可能不存在，不算失败）
    let auth = switch::auth_file_path();
    let mut layers: Vec<&str> = Vec::new();
    if auth.exists() {
        copy_tree(&auth, &root.join("L1").join("auth.info"))?;
        layers.push("L1");
    }

    // L2：账号级目录
    let mut l2_saved = false;
    for suffix in ACCOUNT_SUFFIXES {
        let source = storage_dir().join(format!("user-{uid}{suffix}"));
        if source.exists() {
            copy_tree(&source, &root.join("L2").join(format!("user-{uid}{suffix}")))?;
            l2_saved = true;
        }
    }
    if l2_saved {
        layers.push("L2");
    }

    // L3：Chromium 会话
    let mut session_kb = 0u64;
    if session_dir().exists() {
        save_session(&session_dir(), &root.join("L3").join("session"))?;
        session_kb = dir_size_kb(&root.join("L3").join("session"));
        layers.push("L3");
    }

    // L4：侧边栏列表 + 当前会话路由
    let ui_source = account_dir(uid);
    let mut l4_saved = false;
    for file in ["sidebar-list-snapshot.json", "startup-route.json"] {
        let source = ui_source.join(file);
        if source.exists() {
            copy_tree(&source, &root.join("L4").join(file))?;
            l4_saved = true;
        }
    }
    if l4_saved {
        layers.push("L4");
    }

    if layers.is_empty() {
        return Err("没有找到任何可快照的本地数据（客户端是否从未登录过？）".to_string());
    }

    let meta = json!({
        "uid": uid,
        "label": if label.trim().is_empty() { uid.to_string() } else { label.trim().to_string() },
        "savedAt": now_ms(),
        "sessionBytes": session_kb.saturating_mul(1024),
        "layers": layers,
    });
    let text = serde_json::to_string_pretty(&meta).map_err(|error| error.to_string())?;
    crate::paths::write_private(&root.join("meta.json"), &text)
        .map_err(|error| format!("写入快照信息失败：{error}"))?;

    Ok(meta)
}

/// 恢复一个账号的现场（L2/L3/L4）。L1 由切换流程自己写入，因为账号库里的 token
/// 比快照里的更新。
///
/// 目标账号没有快照时返回 `restored = false` 与原因，由界面提示，不当作错误。
pub fn restore(uid: &str) -> Result<Value, String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Ok(json!({ "restored": false, "reason": "缺少账号 uid" }));
    }
    let root = snapshot_dir(uid);
    if !root.exists() {
        return Ok(json!({
            "restored": false,
            "reason": "该账号还没有保存过快照，本次只写入登录态，会话可能中断。建议登录该账号后点一次「保存现场」。"
        }));
    }

    let mut restored: Vec<&str> = Vec::new();

    // L2
    for suffix in ACCOUNT_SUFFIXES {
        let name = format!("user-{uid}{suffix}");
        let from = root.join("L2").join(&name);
        if from.exists() {
            copy_tree(&from, &storage_dir().join(&name))?;
            if !restored.contains(&"L2") {
                restored.push("L2");
            }
        }
    }

    // L3
    let session_from = root.join("L3").join("session");
    if session_from.exists() {
        restore_session(&session_from, &session_dir())?;
        restored.push("L3");
    }

    // L4
    let target_ui = account_dir(uid);
    let mut l4 = false;
    for file in ["sidebar-list-snapshot.json", "startup-route.json"] {
        let from = root.join("L4").join(file);
        if from.exists() {
            copy_tree(&from, &target_ui.join(file))?;
            l4 = true;
        }
    }
    if l4 {
        restored.push("L4");
    }

    Ok(json!({
        "restored": !restored.is_empty(),
        "layers": restored,
        "savedAt": read_meta(&root).get("savedAt").cloned().unwrap_or(Value::Null),
    }))
}

fn read_meta(root: &Path) -> Value {
    std::fs::read_to_string(root.join("meta.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}))
}

/// 列出所有快照。
pub fn list() -> Value {
    let root = profiles_root();
    let mut items: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let meta = read_meta(&entry.path());
            items.push(json!({
                "uid": meta.get("uid").cloned().unwrap_or_else(|| json!(entry.file_name().to_string_lossy())),
                "label": meta.get("label").cloned().unwrap_or(Value::Null),
                "savedAt": meta.get("savedAt").cloned().unwrap_or(Value::Null),
                "sessionBytes": meta.get("sessionBytes").cloned().unwrap_or(json!(0)),
                "layers": meta.get("layers").cloned().unwrap_or(json!([])),
            }));
        }
    }
    items.sort_by(|a, b| {
        let left = b.get("savedAt").and_then(Value::as_i64).unwrap_or(0);
        let right = a.get("savedAt").and_then(Value::as_i64).unwrap_or(0);
        left.cmp(&right)
    });
    json!({ "root": root.to_string_lossy(), "items": items })
}

pub fn delete(uid: &str) -> Result<(), String> {
    let root = snapshot_dir(uid);
    if !root.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(&root).map_err(|error| format!("删除快照失败：{error}"))
}

/// 从官方登录态文件里读出当前登录账号的 uid（切换前用来确定"要存谁的现场"）。
pub fn current_uid() -> Option<String> {
    let text = std::fs::read_to_string(switch::auth_file_path()).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .pointer("/account/uid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|uid| !uid.is_empty())
        .map(String::from)
}

/// 账号库里的昵称，用于快照元信息。
pub fn label_of(uid: &str) -> String {
    crate::store::load_accounts()
        .into_iter()
        .find(|account| crate::store::get_str(account, "uid").as_deref() == Some(uid))
        .and_then(|account| crate::store::get_str(&account, "nickname"))
        .unwrap_or_else(|| uid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn safe_name_strips_path_separators() {
        assert_eq!(safe_name("../../etc/passwd"), "______etc_passwd");
        assert_eq!(safe_name("7d592d8a-11df"), "7d592d8a-11df");
        assert_eq!(safe_name("  "), "unknown");
    }

    #[test]
    fn cache_dirs_are_excluded_but_state_is_not() {
        assert!(is_excluded("Code Cache"));
        assert!(is_excluded("Cache"));
        // 这些是登录态本体，绝不能跳过
        for keep in ["Cookies", "Local Storage", "Partitions", "IndexedDB"] {
            assert!(!is_excluded(keep), "{keep} 不应被排除");
        }
    }

    #[test]
    fn meta_without_saved_at_is_tolerated() {
        assert_eq!(read_meta(Path::new("/nonexistent")), json!({}));
    }
}
