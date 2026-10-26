//! 账号切换：把账号库里的某个账号写进官方客户端的登录态，并可选择重启客户端。
//!
//! 官方登录态文件结构固定为四个键：`account` / `accounts` / `allAccounts` / `auth`。
//! 切换**不是**凭空构造一份新文件，而是以现有文件为基底：
//!   - 现有 `allAccounts` 原样保留——官方写入的条目含 `uin` / `oneidAccountId` /
//!     `idp` 等我们账号库里没有的字段，重新构造会丢失，导致切回来的账号资料残缺；
//!   - 当前登录账号的 `account` 也并回 `allAccounts`，使「切走再切回」仍能保真；
//!   - 只有目标账号的 `auth` 与 `account` 被替换。
//!
//! 风险：该操作会覆写官方登录态；`restart = true` 时会关闭并重新启动 WorkBuddy，
//! 用户此刻在客户端里的会话会中断。界面必须在执行前明确告知。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::api;
use crate::paths::{data_dir, now_ms};
use crate::store;

/// 官方客户端 bundle id，用于 osascript 优雅退出。
const CLIENT_BUNDLE_ID: &str = "com.tencent.workbuddy.mac";
/// 主进程命令行特征。用完整路径而非可执行名（`Electron`）匹配，
/// 避免误伤同机其它 Electron 应用。
const MAIN_PROCESS_PATTERN: &str = "/Applications/WorkBuddy.app/Contents/MacOS/Electron";
/// 包内辅助进程特征（GPU/渲染/网络等）。
const HELPER_PROCESS_PATTERN: &str = "WorkBuddy Helper";
/// 客户端安装路径候选。
const CLIENT_APP_PATHS: &[&str] = &[
    "/Applications/WorkBuddy.app",
    "/Applications/CodeBuddy CN.app",
];

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// 官方登录态文件路径。
pub fn auth_file_path() -> PathBuf {
    home_dir()
        .join("Library/Application Support/CodeBuddyExtension/Data/Public/auth")
        .join("workbuddy-desktop.info")
}

/// 切换前的备份目录。
fn backup_dir() -> PathBuf {
    data_dir().join("backups")
}

fn client_app_path() -> Option<PathBuf> {
    CLIENT_APP_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

// ---------------------------------------------------------------------------
// 子进程
// ---------------------------------------------------------------------------

/// 运行命令并在超时后杀掉。与「先等退出再读输出」不同，这里并发读取管道，
/// 避免输出超过管道缓冲区时子进程写满阻塞、永不退出。
fn run_cmd(program: &str, args: &[&str], timeout_secs: u64) -> Option<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = std::process::Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;
    let stdout_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    status.map(|status| std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn matching_pids(pattern: &str) -> Vec<String> {
    run_cmd("pgrep", &["-f", pattern], 5)
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// 客户端主进程是否在运行。
pub fn client_running() -> bool {
    !matching_pids(MAIN_PROCESS_PATTERN).is_empty()
}

fn wait_for_processes_gone(pattern: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matching_pids(pattern).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 关闭客户端：先 osascript 优雅退出，再清理包内残留进程。
///
/// 切换与「保存现场」共用：两处都要求客户端完全退出后才能动 session 目录。
pub(crate) fn close_client(timeout_secs: u64) -> Result<(), String> {
    if !client_running() {
        return Ok(());
    }

    let script = format!("quit app id \"{CLIENT_BUNDLE_ID}\"");
    let _ = run_cmd("osascript", &["-e", script.as_str()], 15);

    if wait_for_processes_gone(MAIN_PROCESS_PATTERN, Duration::from_secs(8)) {
        // 主进程已退，给辅助进程一点收尾时间
        let _ = wait_for_processes_gone(HELPER_PROCESS_PATTERN, Duration::from_secs(4));
        return Ok(());
    }

    for pattern in [HELPER_PROCESS_PATTERN, MAIN_PROCESS_PATTERN] {
        let _ = run_cmd("pkill", &["-f", pattern], 10);
    }

    let budget = Duration::from_secs(timeout_secs.clamp(5, 60));
    if wait_for_processes_gone(MAIN_PROCESS_PATTERN, budget) {
        let _ = wait_for_processes_gone(HELPER_PROCESS_PATTERN, Duration::from_secs(4));
        return Ok(());
    }

    let leftover = matching_pids(MAIN_PROCESS_PATTERN);
    Err(format!(
        "无法完全关闭 WorkBuddy（残留进程 {}）。请手动退出客户端后重试。",
        leftover.join(", ")
    ))
}

pub(crate) fn launch_client() -> Result<(), String> {
    let app = client_app_path().ok_or_else(|| {
        format!(
            "未找到 WorkBuddy 客户端（尝试路径：{}）",
            CLIENT_APP_PATHS.join("、")
        )
    })?;

    let output = run_cmd("open", &[app.to_string_lossy().as_ref()], 15)
        .ok_or_else(|| "启动 WorkBuddy 超时".to_string())?;
    if !output.status.success() {
        return Err(format!(
            "启动 WorkBuddy 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 登录态文件读写
// ---------------------------------------------------------------------------

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn permission_hint(error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        "无权限写入官方登录态文件。请到「系统设置 → 隐私与安全性 → App 管理」允许本 App 控制 WorkBuddy 的数据后重试。".to_string()
    } else {
        format!("写入官方登录态文件失败：{error}")
    }
}

/// 原子写：写临时文件 → 继承原文件权限 → rename。
///
/// 保留权限而不是固定 0600：这个文件由官方客户端创建与读取，权限应与客户端
/// 自己写入时一致，避免因权限差异引起客户端异常。
fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    let mode = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "workbuddy-desktop.info".to_string());
    let tmp = path.with_file_name(format!("{file_name}.tmp-{}", std::process::id()));

    std::fs::write(&tmp, content).map_err(|error| permission_hint(&error))?;
    if let Some(permissions) = mode {
        let _ = std::fs::set_permissions(&tmp, permissions);
    }
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        permission_hint(&error)
    })
}

fn set_default(object: &mut serde_json::Map<String, Value>, key: &str, value: Value) {
    if !object.contains_key(key) {
        object.insert(key.to_string(), value);
    }
}

/// 由基底构造官方 `account` 对象。
///
/// 基底优先级由调用方决定（账号库原件 → 官方文件同 uid 条目 → 空对象兜底）。
/// 这里只补齐官方客户端读取的键，缺失会导致账号卡片显示异常。
fn build_account_object(base: &Value, account: &Value) -> Value {
    let mut object = base.as_object().cloned().unwrap_or_default();

    // uid 是官方识别账号的主键，必须与账号库一致；为空时不覆盖基底里的值。
    if let Some(uid) = store::get_str(account, "uid") {
        object.insert("uid".to_string(), json!(uid));
    }

    // 账号库的值只在基底没有时才生效：官方原始资料里的昵称/手机号是权威的，
    // 强行覆盖可能把昵称写成手机号。但键本身必须存在——客户端会直接读取。
    let nickname = store::get_str(account, "nickname").unwrap_or_default();
    set_default(&mut object, "nickname", json!(nickname));

    let phone = store::get_str(account, "phoneNumber")
        .or_else(|| store::get_str(account, "phone"))
        .unwrap_or_default();
    set_default(&mut object, "phoneNumber", json!(phone));

    set_default(&mut object, "type", json!("personal"));
    set_default(&mut object, "accountType", json!(""));
    set_default(&mut object, "idp", json!(""));
    set_default(&mut object, "oneidAccountId", json!(""));
    set_default(&mut object, "areaInfoComplete", json!(false));
    set_default(&mut object, "isCurrentOneIdEnterprise", json!(false));
    set_default(&mut object, "isCurrentOneIdPersonal", json!(false));
    set_default(&mut object, "isFirstLogin", json!(false));
    set_default(&mut object, "isCreator", json!(false));
    set_default(&mut object, "isAdmin", json!(false));
    set_default(&mut object, "uin", json!(""));
    set_default(&mut object, "lastLogin", json!(true));
    set_default(&mut object, "pluginEnabled", json!(true));
    set_default(
        &mut object,
        "deployStatus",
        json!({"statusCode": 0, "statusMsg": "", "detailMsg": ""}),
    );
    set_default(
        &mut object,
        "sso",
        json!({"domain": "", "domainModifiedTimes": 0}),
    );

    Value::Object(object)
}

/// 由账号记录构造官方 `auth` 对象。
fn build_auth_object(account: &Value) -> Value {
    let now = now_ms();
    let mut object = serde_json::Map::new();

    object.insert(
        "accessToken".to_string(),
        json!(store::get_str(account, "access_token").unwrap_or_default()),
    );
    object.insert(
        "refreshToken".to_string(),
        json!(store::get_str(account, "refresh_token").unwrap_or_default()),
    );
    object.insert(
        "tokenType".to_string(),
        json!(store::get_str(account, "token_type").unwrap_or_else(|| "Bearer".to_string())),
    );
    object.insert(
        "domain".to_string(),
        json!(store::get_str(account, "domain").unwrap_or_else(|| "www.codebuddy.cn".to_string())),
    );
    object.insert(
        "scope".to_string(),
        json!("openid profile offline_access email"),
    );
    object.insert("lastRefreshTime".to_string(), json!(now));
    object.insert("notBeforePolicy".to_string(), json!(0));
    object.insert("sessionState".to_string(), json!(""));

    // expiresAt 必须原样写入：客户端靠它判断是否需要刷新。剩余秒数用 max(0)，
    // 已过期时写 0 让客户端立即走刷新流程。
    for (at_key, in_key) in [
        ("expiresAt", "expiresIn"),
        ("refreshExpiresAt", "refreshExpiresIn"),
    ] {
        match account.get(at_key).and_then(Value::as_i64) {
            Some(value) => {
                object.insert(at_key.to_string(), json!(value));
                object.insert(in_key.to_string(), json!(((value - now) / 1000).max(0)));
            }
            None => {
                object.insert(in_key.to_string(), json!(0));
            }
        }
    }

    Value::Object(object)
}

/// 组装完整的登录态内容。
fn build_session(account: &Value, existing: &Value) -> Value {
    let target_uid = store::get_str(account, "uid").unwrap_or_default();

    let mut all: Vec<Value> = existing
        .get("allAccounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // 把当前登录账号原样并回历史列表：官方写入的条目字段最完整。
    if let Some(current) = existing
        .get("account")
        .filter(|value| value.is_object())
        .cloned()
    {
        let current_uid = store::get_str(&current, "uid").unwrap_or_default();
        let duplicate = !current_uid.is_empty()
            && all
                .iter()
                .any(|item| store::get_str(item, "uid").as_deref() == Some(current_uid.as_str()));
        if !duplicate {
            all.push(current);
        }
    }

    // 目标账号的基底：账号库保存的原始资料优先，其次是历史列表里同 uid 的条目。
    let base = account
        .get("profile_raw")
        .filter(|value| value.is_object())
        .cloned()
        .or_else(|| {
            all.iter()
                .find(|item| {
                    !target_uid.is_empty()
                        && store::get_str(item, "uid").as_deref() == Some(target_uid.as_str())
                })
                .cloned()
        })
        .unwrap_or_else(|| json!({}));

    let account_object = build_account_object(&base, account);
    let auth_object = build_auth_object(account);

    // 同一个 uid 只保留一条，避免历史列表里出现重名项。
    if !target_uid.is_empty() {
        all.retain(|item| store::get_str(item, "uid").as_deref() != Some(target_uid.as_str()));
    }
    all.push(account_object.clone());

    let all_value = Value::Array(all);
    json!({
        "account": account_object,
        "auth": auth_object,
        "accounts": all_value.clone(),
        "allAccounts": all_value,
    })
}

/// 备份当前登录态，返回备份文件路径。
fn backup_auth_file() -> Result<Option<PathBuf>, String> {
    let path = auth_file_path();
    if !path.exists() {
        return Ok(None);
    }

    std::fs::create_dir_all(backup_dir()).map_err(|error| format!("创建备份目录失败：{error}"))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let dest = backup_dir().join(format!("workbuddy-desktop.{stamp}.info"));

    std::fs::copy(&path, &dest).map_err(|error| format!("备份登录态失败：{error}"))?;
    Ok(Some(dest))
}

fn write_auth_file(account: &Value) -> Result<PathBuf, String> {
    let path = auth_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建登录态目录失败：{error}"))?;
    }

    let existing = read_json(&path).unwrap_or_else(|| json!({}));
    let session = build_session(account, &existing);
    let content = serde_json::to_string_pretty(&session).map_err(|error| error.to_string())?;
    atomic_write(&path, &content)?;

    // 写后校验：rename 成功不等于内容正确，必须回读目标账号的 token 比对。
    let written = read_json(&path).ok_or_else(|| "写入后无法重新读取登录态文件".to_string())?;
    let written_token = written
        .pointer("/auth/accessToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    let expected = store::get_str(account, "access_token").unwrap_or_default();
    if written_token != expected {
        return Err("登录态写后校验失败，未写入目标账号".to_string());
    }

    Ok(path)
}

/// 只替换凭证：`auth` 换成目标账号的 token，`account` / `accounts` / `allAccounts`
/// 保持原样。
///
/// 用途是只替换凭证、保留原登录身份：客户端里显示的仍是原来那个人，
/// 但每次模型调用带的是目标账号的 token，服务端按目标账号扣积分。
fn write_auth_file_credits_only(account: &Value) -> Result<PathBuf, String> {
    let path = auth_file_path();
    let mut session = read_json(&path)
        .ok_or_else(|| "读不到官方登录态文件，请先在客户端登录一次".to_string())?;

    if session.get("account").and_then(Value::as_object).is_none() {
        return Err("登录态里没有当前账号信息，无法保留登录身份".to_string());
    }

    // 写前记下被保留的身份，写后回读校验没有被顺手改掉
    let keep_uid = session
        .pointer("/account/uid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let access = store::get_str(account, "access_token").unwrap_or_default();
    if access.is_empty() {
        return Err("该账号缺少 access_token，请重新导入".to_string());
    }
    // 目标 token 已过期就别写：写进去客户端只会刷新失败，进而把当前登录态一起弄掉。
    if let Some(expire) = jwt_exp(&access) {
        let now_secs = now_ms() / 1000;
        if expire <= now_secs {
            return Err("目标账号的凭证已过期，请先在 WB-Checkin 重新导入该账号".to_string());
        }
    }

    // 只覆盖凭证字段。`domain` / `scope` / `sessionState` 必须沿用当前文件里的值：
    // 账号库里存的是导入那一刻的旧值（codebuddy.cn），而客户端实际用的是
    // workbuddy.cn，写回去会让请求打到错误的环境。
    let mut fresh = build_auth_object(account);
    // 账号库里的 `expiresAt` 是导入那一刻的旧值，刷新过 token 之后不会同步，
    // 于是会早于 token 自身的 `exp`。以 JWT 的 exp 为准，否则客户端会提前两天
    // 误判过期、触发一次不必要的刷新（刷新失败就直接掉登录）。
    if let Some(expire) = jwt_exp(&access) {
        fresh["expiresAt"] = json!(expire * 1000);
        fresh["expiresIn"] = json!((expire - now_ms() / 1000).max(0));
    }
    let auth = session
        .get_mut("auth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "登录态里没有 auth 段，无法替换凭证".to_string())?;
    for key in [
        "accessToken",
        "refreshToken",
        "tokenType",
        "expiresAt",
        "expiresIn",
        "refreshExpiresAt",
        "refreshExpiresIn",
        "lastRefreshTime",
    ] {
        if let Some(value) = fresh.get(key) {
            auth.insert(key.to_string(), value.clone());
        }
    }

    let content = serde_json::to_string_pretty(&session).map_err(|error| error.to_string())?;
    atomic_write(&path, &content)?;

    let written = read_json(&path).ok_or_else(|| "写入后无法重新读取登录态文件".to_string())?;
    let written_token = written
        .pointer("/auth/accessToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    let expected = store::get_str(account, "access_token").unwrap_or_default();
    if written_token != expected {
        return Err("写后校验失败：凭证未替换成目标账号".to_string());
    }
    let written_uid = written
        .pointer("/account/uid")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !keep_uid.is_empty() && written_uid != keep_uid {
        return Err("写后校验失败：登录身份被改动".to_string());
    }

    // 目标就是当前登录账号时，等于切回自己付钱，旁注要清掉。
    let target_uid = store::get_str(account, "uid").unwrap_or_default();
    if target_uid == keep_uid {
        clear_credits_target();
    } else {
        let name = store::get_str(account, "nickname").unwrap_or_default();
        save_credits_target(&target_uid, &name);
    }

    Ok(path)
}

// ---------------------------------------------------------------------------
// 登录身份 / 付费账号
// ---------------------------------------------------------------------------

/// base64url 解码（不带填充）。只用于读 JWT 的 payload，不涉及验签。
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut lookup = [255u8; 256];
    for (index, byte) in TABLE.iter().enumerate() {
        lookup[*byte as usize] = index as u8;
    }

    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for byte in input.bytes() {
        let value = lookup[byte as usize];
        if value == 255 {
            return None;
        }
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// 从 accessToken（JWT）里取出 `sub` 与 `preferred_username`。
///
/// 不验签：这里只是读「这个 token 是谁的」，用于界面上分辨付费账号，以及独立登录
/// 拿不到账号资料时兜底认出 uid（避免同一个账号被记成两条）。
pub(crate) fn token_owner(token: &str) -> Option<(String, Option<String>)> {
    let payload = token.split('.').nth(1)?;
    let raw = base64url_decode(payload)?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let sub = value.get("sub")?.as_str()?.trim().to_string();
    if sub.is_empty() {
        return None;
    }
    let username = value
        .get("preferred_username")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((sub, username))
}

/// JWT 的 `exp`（秒）。只读 payload，不验签；不是 JWT 时返回 None。
fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let raw = base64url_decode(payload)?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    value.get("exp").and_then(Value::as_i64)
}

/// 把官方客户端**当前登录态**里的凭证同步回账号库。
///
/// 存在的原因：服务端刷新一次就换发新的 refresh token，客户端刷新过之后账号库里
/// 那份副本就被轮换掉了，签到再去刷必然 12153。只要客户端此刻登录的正是这个账号，
/// 就可以把它的最新凭证原样取回来继续用，不必让用户重新登录。
///
/// 只读客户端文件，不写、不重启；客户端没登录这个账号时返回错误，由调用方决定是否
/// 落到「需要重新登录」。
pub fn sync_from_client(id: &str, uid: &str) -> Result<Value, String> {
    let session = read_json(&auth_file_path()).ok_or_else(|| "读不到官方登录态文件".to_string())?;

    let login_uid = session
        .pointer("/account/uid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let auth = session
        .get("auth")
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let access_token = auth
        .get("accessToken")
        .or_else(|| auth.get("access_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "客户端登录态里没有可用的 access token".to_string())?;

    // 归属校验：token 能解析就用它的 `sub`，解析不了（被客户端加密回写）就退回文件里的 uid。
    let owner_uid = token_owner(&access_token).map(|(sub, _)| sub).or(login_uid);
    match owner_uid.as_deref() {
        Some(owner) if owner == uid => {}
        Some(_) => return Err("客户端当前登录的不是这个账号".to_string()),
        None => return Err("无法确认客户端登录的是哪个账号".to_string()),
    }

    let mut patch = serde_json::Map::new();
    patch.insert("access_token".to_string(), json!(access_token));
    let refresh_token = auth
        .get("refreshToken")
        .or_else(|| auth.get("refresh_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(token) = refresh_token.as_deref() {
        patch.insert("refresh_token".to_string(), json!(token));
    }

    // 过期时间一律以 JWT 的 exp 为准：客户端文件里的 expiresAt 可能已被加密或落在
    // 别的信封里，而 exp 是唯一不会说谎的那个。
    if let Some(expire) = jwt_exp(patch.get("access_token").and_then(Value::as_str).unwrap_or_default())
    {
        patch.insert("expiresAt".to_string(), json!(expire * 1000));
    }
    if let Some(expire) = refresh_token.as_deref().and_then(jwt_exp) {
        patch.insert("refreshExpiresAt".to_string(), json!(expire * 1000));
    }

    patch.insert("refreshedAt".to_string(), json!(now_ms()));
    // 凭证回到客户端手里，刷新链的主动权也随之回到客户端。
    patch.insert("token_owner".to_string(), json!(store::OWNER_CLIENT));
    patch.insert("needs_relogin".to_string(), json!(false));
    patch.insert("needs_relogin_reason".to_string(), Value::Null);

    store::apply_patch(id, &Value::Object(patch))?;
    Ok(json!({ "ok": true, "uid": uid }))
}

/// 记录「额度切给了谁」——客户端会把明文 token 重新加密回写，那时就无法再从文件里
/// 读出 token 的归属了，只能靠这份旁注继续在界面上标出付费账号。
fn credits_target_path() -> PathBuf {
    crate::paths::data_dir().join("credits-target.json")
}

fn save_credits_target(uid: &str, name: &str) {
    if uid.trim().is_empty() {
        return;
    }
    let value = json!({
        "uid": uid,
        "name": name,
        "at": now_ms(),
    });
    if let Ok(content) = serde_json::to_string_pretty(&value) {
        let _ = crate::paths::write_private(&credits_target_path(), &content);
    }
}

fn clear_credits_target() {
    let _ = std::fs::remove_file(credits_target_path());
}

/// 当前「谁在登录」与「谁在付费」。
///
/// 两者通常相同；用「只换额度」切过之后，`account.uid` 仍是原账号，而 token 已经换成
/// 目标账号的，于是二者分离 —— 界面必须能分别显示，否则用户看不出积分扣的是谁。
pub fn identity() -> Value {
    let session = read_json(&auth_file_path()).unwrap_or_else(|| json!({}));
    let login_uid = session
        .pointer("/account/uid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let token = session
        .pointer("/auth/accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut owner = token_owner(token);

    // 客户端会把明文 token 重新加密回写（`$wbEncrypted` 信封），那时从文件里读不出
    // 归属，只能退回切换时留下的旁注。
    if owner.is_none() {
        if let Some(marked) = read_json(&credits_target_path()) {
            let uid = store::get_str(&marked, "uid").unwrap_or_default();
            if !uid.is_empty() {
                owner = Some((uid, store::get_str(&marked, "name")));
            }
        }
    }

    // 付费账号与登录账号相同时不必单独标注，否则界面上会并排出现两个徽标。
    let billing = owner.filter(|(uid, _)| login_uid.as_deref() != Some(uid.as_str()));

    json!({
        "loginUid": login_uid,
        "billingUid": billing.as_ref().map(|(uid, _)| uid.clone()),
        "billingName": billing.and_then(|(_, name)| name),
    })
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 切换前的环境检查：让界面在执行前就知道会缺什么。
pub fn preflight() -> Value {
    let path = auth_file_path();
    let app = client_app_path();

    // 只探测目录可写性，不实际创建目标文件：目标文件可能尚不存在，
    // 而「目录可写」才是决定成败的那一项。
    let writable = match path.parent() {
        Some(dir) if dir.exists() => {
            let probe = dir.join(format!(".wb-checkin-probe-{}", std::process::id()));
            match std::fs::File::create(&probe) {
                Ok(_) => {
                    let _ = std::fs::remove_file(&probe);
                    true
                }
                Err(_) => false,
            }
        }
        _ => false,
    };

    json!({
        "authFilePath": path.to_string_lossy(),
        "authFileExists": path.exists(),
        "authFileWritable": writable,
        "clientAppPath": app.as_ref().map(|path| path.to_string_lossy().to_string()),
        "clientInstalled": app.is_some(),
        "clientRunning": client_running(),
        "backupDir": backup_dir().to_string_lossy(),
    })
}

/// 执行切换。
///
/// `restart` 为真时关闭并重新启动客户端；为假时只写登录态，由用户自行重启。
/// `keep_session` 为真时走分层快照：先把当前账号的四层现场存档，再写入目标账号的
/// 登录态，最后恢复目标账号的现场 —— 这样重启后侧边栏与正在进行的会话都还在。
/// `credits_only` 为真时只替换 `auth` 凭证：登录身份与本地现场一律不动，
/// 客户端仍显示原账号，结算走凭证所属的那个账号。
///
/// `force` 为真时允许把签到侧独立持有的账号写进客户端。默认拒绝：写进去之后客户端
/// 一刷新就会把 refresh token 轮换掉，签到随之失效。
pub async fn switch_account(
    id: &str,
    restart: bool,
    keep_session: bool,
    credits_only: bool,
    force: bool,
) -> Result<Value, String> {
    let account = store::load_accounts()
        .into_iter()
        .find(|account| store::get_str(account, "id").as_deref() == Some(id))
        .ok_or_else(|| "账号不存在".to_string())?;

    let label = store::get_str(&account, "nickname")
        .or_else(|| store::get_str(&account, "uid"))
        .unwrap_or_else(|| "未知账号".to_string());

    // 签到侧的凭证一旦交给客户端，客户端刷新一次就会把它轮换掉，账号库里的副本当场
    // 作废。这类损耗是静默的（客户端里看着一切正常），必须在写入前拦下来。
    if !force && store::chain_owner(&account) == store::OWNER_CHECKIN {
        return Err(format!(
            "「{label}」的 token 由签到侧独立持有：写进客户端后，客户端刷新一次就会把它轮换掉，自动签到会立即失效。\
             要切这个账号，请先在官方客户端登录它（登录后的链归客户端持有，可正常切换），\
             或在界面上勾选「强制切换」并接受该账号自动签到失效。"
        ));
    }

    if store::get_str(&account, "access_token").is_none() {
        return Err("该账号缺少 access_token，请重新导入或登录".to_string());
    }

    // 写 L1 之后「当前登录的是谁」就查不到了，先把 uid 记下来，用于存档。
    let previous_uid = crate::profile::current_uid();

    // 先把 token 刷新到最新再写入：否则客户端继位后会拿着一个临期（甚至已过期）
    // 的 token 启动。刷新失败不阻断切换，但要在结果里说明。
    let (account, refreshed, refresh_error) = match api::refresh_guarded(&account).await {
        Ok(patch) => {
            let mut merged = account.clone();
            if let (Some(target), Some(source)) = (merged.as_object_mut(), patch.as_object()) {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
            store::apply_patch(id, &patch)?;
            (merged, true, None)
        }
        Err(error) => (account, false, Some(error)),
    };

    let backup = backup_auth_file()?;

    if restart {
        close_client(20)?;
    }

    // 存当前账号的现场必须放在客户端退出之后：运行中复制 session 会拿到写了一半的
    // Cookie。存档失败不阻断切换，但要在结果里说明。
    let mut saved_profile: Option<Value> = None;
    let mut save_error: Option<String> = None;
    let mut profile_restore = Value::Null;

    let path = if credits_only {
        // 只换凭证：不存档、不恢复任何现场层，登录身份保持原样。
        write_auth_file_credits_only(&account)?
    } else {
        if keep_session {
            match previous_uid.as_deref() {
                Some(uid) => match crate::profile::save(uid, &crate::profile::label_of(uid)) {
                    Ok(meta) => saved_profile = Some(meta),
                    Err(error) => save_error = Some(error),
                },
                None => save_error = Some("读不到当前登录账号，无法为它存档".to_string()),
            }
        }

        let path = write_auth_file(&account)?;

        // 登录态写成功后恢复目标账号的现场：L2 用户目录、L3 Chromium 会话、L4 侧边栏与
        // 当前会话路由。目标没有快照时返回原因，不当作失败。
        if keep_session {
            let target_uid = store::get_str(&account, "uid").unwrap_or_default();
            profile_restore = crate::profile::restore(&target_uid)
                .unwrap_or_else(|error| json!({ "restored": false, "reason": error }));
        }

        // 把构造出的完整账号对象存回账号库，下次切换/导入时保真度更高。
        let session = read_json(&path).unwrap_or_else(|| json!({}));
        if let Some(object) = session.get("account").filter(|value| value.is_object()) {
            let _ = store::apply_patch(id, &json!({ "profile_raw": object.clone() }));
        }
        path
    };

    // 凭证已经落进客户端文件，刷新链的主动权随之转移：此后客户端每刷新一次都会换发
    // 新的 refresh token，账号库里这份只能被动失效。标记清楚，免得下次又指望它还能刷。
    let _ = store::apply_patch(id, &json!({ "token_owner": store::OWNER_CLIENT }));

    let mut launched = false;
    let mut launch_error: Option<String> = None;
    if restart {
        match launch_client() {
            Ok(()) => launched = true,
            Err(error) => launch_error = Some(error),
        }
    }

    Ok(json!({
        "ok": true,
        "account": label,
        "authFilePath": path.to_string_lossy(),
        "backup": backup.map(|path| path.to_string_lossy().to_string()),
        "refreshed": refreshed,
        "refreshError": refresh_error,
        "restarted": launched,
        "launchError": launch_error,
        "keptSession": keep_session && !credits_only,
        "creditsOnly": credits_only,
        "savedProfile": saved_profile,
        "saveError": save_error,
        "profileRestore": profile_restore,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(uid: &str, nickname: &str) -> Value {
        json!({
            "id": format!("uid:{uid}"),
            "uid": uid,
            "nickname": nickname,
            "domain": "www.codebuddy.cn",
            "access_token": format!("token-{uid}"),
            "refresh_token": format!("refresh-{uid}"),
            "expiresAt": now_ms() + 5_184_000_000i64,
            "refreshExpiresAt": now_ms() + 6_048_000_000i64,
        })
    }

    #[test]
    fn session_keeps_existing_accounts_and_preserves_current_account() {
        // 现有文件里有一个官方写入的账号（字段很全），切到另一个账号后它不能被丢掉
        let existing = json!({
            "account": {
                "uid": "official-uid",
                "nickname": "官方昵称",
                "uin": "123456789",
                "oneidAccountId": "oneid-1"
            },
            "auth": { "accessToken": "old" },
            "allAccounts": [
                { "uid": "official-uid", "nickname": "官方昵称", "uin": "123456789" }
            ]
        });

        let session = build_session(&account("target-uid", "目标账号"), &existing);
        let all = session
            .get("allAccounts")
            .and_then(Value::as_array)
            .expect("应有 allAccounts");
        let uids: Vec<&str> = all
            .iter()
            .filter_map(|item| item.get("uid").and_then(Value::as_str))
            .collect();

        assert!(uids.contains(&"official-uid"), "原账号必须保留：{uids:?}");
        assert!(uids.contains(&"target-uid"), "目标账号必须写入：{uids:?}");
        assert_eq!(
            session.pointer("/auth/accessToken").and_then(Value::as_str),
            Some("token-target-uid")
        );
    }

    #[test]
    fn session_does_not_duplicate_same_uid() {
        let existing = json!({
            "account": { "uid": "same-uid", "nickname": "旧昵称" },
            "allAccounts": [
                { "uid": "same-uid", "nickname": "旧昵称" },
                { "uid": "other-uid", "nickname": "另一个" }
            ]
        });

        let session = build_session(&account("same-uid", "新昵称"), &existing);
        let all = session.get("allAccounts").and_then(Value::as_array).unwrap();
        let same_count = all
            .iter()
            .filter(|item| item.get("uid").and_then(Value::as_str) == Some("same-uid"))
            .count();

        assert_eq!(same_count, 1, "同一 uid 只应保留一条");
        // 另外那个账号不受影响
        assert!(all
            .iter()
            .any(|item| item.get("uid").and_then(Value::as_str) == Some("other-uid")));
    }

    #[test]
    fn account_object_fills_keys_the_client_reads() {
        // 基底为空对象时（此前从未在客户端登录过的新账号），必须补出官方会读的键
        let object = build_account_object(&json!({}), &account("u1", "昵称"));

        for key in [
            "uid",
            "nickname",
            "type",
            "accountType",
            "idp",
            "oneidAccountId",
            "areaInfoComplete",
            "isFirstLogin",
            "uin",
            "phoneNumber",
            "lastLogin",
            "pluginEnabled",
            "deployStatus",
            "sso",
        ] {
            assert!(object.get(key).is_some(), "缺少官方读取的键: {key}");
        }
        assert_eq!(object.get("uid").and_then(Value::as_str), Some("u1"));
    }

    #[test]
    fn account_object_prefers_official_base_fields() {
        // 官方原始资料里的昵称应当胜过账号库里的（账号库往往存的是手机号）
        let base = json!({ "uid": "u1", "nickname": "官方昵称", "uin": "998877" });
        let object = build_account_object(&base, &account("u1", "16600000000"));

        assert_eq!(object.get("nickname").and_then(Value::as_str), Some("官方昵称"));
        assert_eq!(object.get("uin").and_then(Value::as_str), Some("998877"));
    }

    #[test]
    fn auth_object_writes_expiry_for_client_side_refresh() {
        let record = account("u1", "昵称");
        let auth = build_auth_object(&record);

        assert_eq!(auth.get("accessToken").and_then(Value::as_str), Some("token-u1"));
        assert_eq!(auth.get("tokenType").and_then(Value::as_str), Some("Bearer"));
        assert_eq!(auth.get("domain").and_then(Value::as_str), Some("www.codebuddy.cn"));
        // expiresAt 必须原样保留，客户端靠它判断是否要刷新
        assert_eq!(
            auth.get("expiresAt").and_then(Value::as_i64),
            record.get("expiresAt").and_then(Value::as_i64)
        );
        assert!(auth.get("expiresIn").and_then(Value::as_i64).unwrap() > 0);
    }

    #[test]
    fn auth_object_marks_expired_token_with_zero_ttl() {
        let mut record = account("u1", "昵称");
        record["expiresAt"] = json!(now_ms() - 60_000);

        let auth = build_auth_object(&record);
        assert_eq!(auth.get("expiresIn").and_then(Value::as_i64), Some(0));
        // 时间点仍要保留，否则客户端无法判断该走刷新
        assert!(auth.get("expiresAt").and_then(Value::as_i64).is_some());
    }

    #[test]
    fn auth_file_path_points_at_official_location() {
        let path = auth_file_path().to_string_lossy().to_string();
        assert!(path.contains("CodeBuddyExtension"), "路径异常: {path}");
        assert!(path.ends_with("workbuddy-desktop.info"), "路径异常: {path}");
    }

    #[test]
    fn token_owner_reads_sub_and_username() {
        let token =
            "x.eyJzdWIiOiJhYmMiLCJwcmVmZXJyZWRfdXNlcm5hbWUiOiIxODMifQ.y";
        assert_eq!(
            token_owner(token),
            Some(("abc".to_string(), Some("183".to_string())))
        );
    }

    #[test]
    fn token_owner_rejects_malformed_input() {
        assert_eq!(token_owner(""), None);
        assert_eq!(token_owner("not-a-jwt"), None);
        // 段数够但 payload 不是合法 JSON
        assert_eq!(token_owner("a.eyJiYWQi.y"), None);
    }
}
