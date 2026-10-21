//! 账号库：读写 ~/.wb-checkin/accounts.json，脱敏视图，以及从官方本机登录态导入。
//!
//! 对外（前端）永远只暴露脱敏视图：不含 access_token / refresh_token。

use serde_json::{json, Value};
use std::sync::Mutex;

use crate::paths::{accounts_file, now_ms, read_json_array, write_private};

/// 进程内串行闸：所有「读 → 改 → 写」都必须经过它。
///
/// 后台自动签到、手动签到、本机导入、独立授权登录都会改写 accounts.json。
/// 不加闸时两个流程交叠会互相覆盖写入 —— 轻则丢一次签到记录，重则丢掉刚登录
/// 进来的账号。下面的 RMW 函数各自持锁，注意不要再互相嵌套调用以免自锁。
static STORE_LOCK: Mutex<()> = Mutex::new(());

fn with_store_lock<T>(action: impl FnOnce() -> T) -> T {
    let _guard = STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    action()
}


pub fn get_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn parse_ts(value: Option<&Value>) -> Option<i64> {
    let raw = match value? {
        Value::String(text) => text.trim().parse::<f64>().ok()? as i64,
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|f| f as i64))?,
        _ => return None,
    };
    if raw < 10_000_000_000 {
        return Some(raw * 1000);
    }
    Some(raw)
}

pub fn load_accounts() -> Vec<Value> {
    read_json_array(&accounts_file())
}

pub fn save_accounts(accounts: &[Value]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&Value::Array(accounts.to_vec()))
        .map_err(|e| e.to_string())?;
    write_private(&accounts_file(), &text).map_err(|e| e.to_string())
}

/// 前端可见字段。刻意不含任何 token。
pub fn view(account: &Value) -> Value {
    json!({
        "id": get_str(account, "id").unwrap_or_default(),
        "uid": get_str(account, "uid"),
        "nickname": get_str(account, "nickname"),
        "email": get_str(account, "email"),
        "domain": get_str(account, "domain").unwrap_or_else(|| "www.codebuddy.cn".to_string()),
        "hasRefreshToken": get_str(account, "refresh_token").is_some(),
        "needsRelogin": account.get("needs_relogin").and_then(Value::as_bool).unwrap_or(false),
        "needsReloginReason": account.get("needs_relogin_reason").cloned().unwrap_or(Value::Null),
        "expiresAt": account.get("expiresAt").cloned().unwrap_or(Value::Null),
        "refreshExpiresAt": account.get("refreshExpiresAt").cloned().unwrap_or(Value::Null),
        "lastCheckinDate": account.get("last_checkin_date").cloned().unwrap_or(Value::Null),
        "lastCheckinResult": account.get("last_checkin_result").cloned().unwrap_or(Value::Null),
        "lastCheckinMessage": account.get("last_checkin_message").cloned().unwrap_or(Value::Null),
        "lastCheckinAt": account.get("last_checkin_at").cloned().unwrap_or(Value::Null),
        "createdAt": account.get("createdAt").cloned().unwrap_or(Value::Null),
    })
}

pub fn view_all() -> Vec<Value> {
    load_accounts().iter().map(view).collect()
}

/// 新增或按 id/uid 合并账号记录。
pub fn upsert(account: Value) -> Result<(), String> {
    with_store_lock(move || {
        let mut accounts = load_accounts();
        let id = get_str(&account, "id");
        let uid = get_str(&account, "uid");

        let position = accounts.iter().position(|existing| {
            (id.is_some() && get_str(existing, "id") == id)
                || (uid.is_some() && get_str(existing, "uid") == uid)
        });

        match position {
            Some(index) => {
                let mut merged = accounts[index].clone();
                if let (Some(target), Some(source)) = (merged.as_object_mut(), account.as_object()) {
                    for (key, value) in source {
                        if value.is_null() {
                            continue;
                        }
                        target.insert(key.clone(), value.clone());
                    }
                }
                accounts[index] = merged;
            }
            None => accounts.push(account),
        }

        save_accounts(&accounts)
    })
}

/// 把签到过程中拿到的新 token 等字段合并回账号记录。
pub fn apply_patch(id: &str, patch: &Value) -> Result<(), String> {
    let Some(patch_object) = patch.as_object() else {
        return Ok(());
    };
    if patch_object.is_empty() {
        return Ok(());
    }
    with_store_lock(|| {
        let mut accounts = load_accounts();
        let mut changed = false;
        for account in accounts.iter_mut() {
            if get_str(account, "id").as_deref() == Some(id) {
                if let Some(target) = account.as_object_mut() {
                    for (key, value) in patch_object {
                        target.insert(key.clone(), value.clone());
                    }
                }
                changed = true;
                break;
            }
        }
        if changed {
            save_accounts(&accounts)?;
        }
        Ok(())
    })
}

/// 写入一次签到结果。
pub fn record_checkin(id: &str, result: &str, message: &str) {
    with_store_lock(|| {
        let mut accounts = load_accounts();
        let mut changed = false;
        for account in accounts.iter_mut() {
            if get_str(account, "id").as_deref() != Some(id) {
                continue;
            }
            if let Some(target) = account.as_object_mut() {
                target.insert("last_checkin_at".to_string(), json!(now_ms()));
                target.insert("last_checkin_date".to_string(), json!(crate::paths::today_local()));
                target.insert("last_checkin_result".to_string(), json!(result));
                target.insert("last_checkin_message".to_string(), json!(message));
                if result == "error" {
                    let message_lower = message.to_lowercase();
                    if message.contains("重新登录")
                        || message.contains("refresh token")
                        || message_lower.contains("unauthorized")
                    {
                        target.insert("needs_relogin".to_string(), json!(true));
                        target.insert("needs_relogin_reason".to_string(), json!(message));
                    }
                } else {
                    target.remove("needs_relogin");
                    target.remove("needs_relogin_reason");
                }
            }
            changed = true;
            break;
        }
        if changed {
            let _ = save_accounts(&accounts);
        }
    });
}

pub fn remove(id: &str) -> Result<(), String> {
    with_store_lock(|| {
        let mut accounts = load_accounts();
        let before = accounts.len();
        accounts.retain(|account| get_str(account, "id").as_deref() != Some(id));
        if accounts.len() == before {
            return Err("账号不存在".to_string());
        }
        save_accounts(&accounts)
    })
}

/// 手动添加：至少需要 access_token。
pub fn add_manual(
    uid: Option<String>,
    nickname: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    domain: Option<String>,
) -> Result<String, String> {
    let access_token = access_token.trim().to_string();
    if access_token.is_empty() {
        return Err("access_token 不能为空".to_string());
    }
    let domain = domain
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "www.codebuddy.cn".to_string());
    if !crate::api::is_allowed_domain(&domain) {
        return Err(format!(
            "域名 {domain} 不在白名单内，只支持 www.codebuddy.cn / www.workbuddy.cn"
        ));
    }

    let id = uid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("uid:{s}"))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let account = json!({
        "id": id,
        "uid": uid.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        "nickname": nickname.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        "domain": domain,
        "access_token": access_token,
        "refresh_token": refresh_token.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        "createdAt": now_ms(),
    });

    upsert(account)?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// 从官方本机登录态导入
// ---------------------------------------------------------------------------

fn auth_file_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home)
        .join("Library/Application Support/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info")
}

/// 把官方认证文件里的一条记录转成账号记录。
fn from_official_record(root: &Value) -> Option<Value> {
    let account_object = root.get("account").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
    let auth_object = root.get("auth").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));

    let access_token = get_str(&auth_object, "accessToken")
        .or_else(|| get_str(&auth_object, "access_token"))
        .or_else(|| get_str(root, "accessToken"))
        .or_else(|| get_str(root, "access_token"))?;

    let uid = get_str(root, "uid")
        .or_else(|| get_str(&account_object, "uid"))
        .or_else(|| get_str(&account_object, "id"));

    let refresh_token = get_str(&auth_object, "refreshToken")
        .or_else(|| get_str(&auth_object, "refresh_token"))
        .or_else(|| get_str(root, "refreshToken"))
        .or_else(|| get_str(root, "refresh_token"));

    let nickname = get_str(root, "nickname")
        .or_else(|| get_str(root, "name"))
        .or_else(|| get_str(&account_object, "nickname"))
        .or_else(|| get_str(&account_object, "label"));

    let email = get_str(root, "email")
        .or_else(|| get_str(&account_object, "email"))
        .or_else(|| get_str(&auth_object, "email"));

    let domain = get_str(root, "domain")
        .or_else(|| get_str(&auth_object, "domain"))
        .unwrap_or_else(|| "www.codebuddy.cn".to_string());

    let id = uid
        .as_deref()
        .map(|s| format!("uid:{s}"))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    Some(json!({
        "id": id,
        "uid": uid,
        "nickname": nickname,
        "email": email,
        "domain": domain,
        "enterpriseId": get_str(root, "enterpriseId")
            .or_else(|| get_str(root, "enterprise_id"))
            .or_else(|| get_str(&account_object, "enterpriseId"))
            .or_else(|| get_str(&account_object, "enterprise_id")),
        "access_token": access_token,
        "refresh_token": refresh_token,
        "expiresAt": parse_ts(root.get("expiresAt").or_else(|| auth_object.get("expiresAt"))),
        "refreshExpiresAt": parse_ts(
            root.get("refreshExpiresAt").or_else(|| auth_object.get("refreshExpiresAt")),
        ),
        "createdAt": now_ms(),
    }))
}

/// 从官方本机认证文件导入：当前登录账号 + allAccounts 里的历史账号。
pub fn import_local() -> Value {
    let path = auth_file_path();
    let Some(root) = crate::paths::read_json_object(&path) else {
        return json!({
            "ok": false,
            "error": format!("未找到官方登录态文件：{}", path.to_string_lossy()),
            "imported": 0,
        });
    };

    let mut candidates: Vec<Value> = Vec::new();
    if let Some(current) = from_official_record(&root) {
        candidates.push(current);
    }
    if let Some(list) = root
        .get("allAccounts")
        .or_else(|| root.get("accounts"))
        .and_then(Value::as_array)
    {
        for entry in list {
            if let Some(record) = from_official_record(entry) {
                candidates.push(record);
            }
        }
    }

    if candidates.is_empty() {
        return json!({
            "ok": false,
            "error": "认证文件里没有可用的 access_token，请先在官方客户端登录一次",
            "imported": 0,
        });
    }

    let mut known: std::collections::HashSet<String> = load_accounts()
        .iter()
        .flat_map(|account| [get_str(account, "id"), get_str(account, "uid")])
        .flatten()
        .collect();

    let mut imported = 0usize;
    let mut merged = 0usize;

    for candidate in candidates {
        let keys: Vec<String> = [get_str(&candidate, "id"), get_str(&candidate, "uid")]
            .into_iter()
            .flatten()
            .collect();

        if keys.iter().any(|key| known.contains(key)) {
            merged += 1;
        } else {
            imported += 1;
        }
        for key in keys {
            known.insert(key);
        }
        let _ = upsert(candidate);
    }

    json!({
        "ok": true,
        "source": path.to_string_lossy(),
        "imported": imported,
        "merged": merged,
    })
}
