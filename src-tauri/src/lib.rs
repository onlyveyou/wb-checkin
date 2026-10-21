//! Tauri 层：命令注册、后台自动签到调度。
//!
//! 设计取舍：不启动任何本地 HTTP 服务，前端只通过 IPC 调用命令。
//! 这样就没有「本地端口无鉴权、可被同机进程或网页调用」的攻击面。

mod api;
mod credits;
mod ferry;
mod growth;
mod localai;
mod logs;
#[cfg(target_os = "macos")]
mod menubar;
#[cfg(not(target_os = "macos"))]
mod menubar {
    /// 非 macOS 平台没有 NSStatusItem，交回纯文本标题处理。
    pub fn apply(_text: &str) -> bool {
        false
    }
}
mod oauth;
mod paths;
mod profile;
mod store;
mod switch;
mod sysmon;

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    menu::MenuEvent,
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};

/// 托盘 id：菜单栏标题要按 id 找回来，改这里就够。
pub(crate) const TRAY_ID: &str = "main-tray";
/// 网速单独占一个菜单栏图标，避免两行文字把硬盘指标挤走。
pub(crate) const NET_TRAY_ID: &str = "net-tray";

/// 单轮闸：签到过程会反复读改写 accounts.json，同一时刻只允许一个签到流程在跑，
/// 否则后台调度与手动签到交叠可能互相覆盖写入。
static CYCLE_RUNNING: AtomicBool = AtomicBool::new(false);

struct CycleGuard;

impl CycleGuard {
    fn acquire() -> Option<Self> {
        CYCLE_RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| CycleGuard)
    }
}

impl Drop for CycleGuard {
    fn drop(&mut self) {
        CYCLE_RUNNING.store(false, Ordering::SeqCst);
    }
}

fn display_name(account: &Value) -> String {
    store::get_str(account, "nickname")
        .or_else(|| store::get_str(account, "email"))
        .or_else(|| store::get_str(account, "uid"))
        .unwrap_or_else(|| "未知账号".to_string())
}

/// 跑一个账号的签到，落库并写日志。返回给前端的单条结果。
async fn perform(account: Value) -> Value {
    let id = store::get_str(&account, "id").unwrap_or_default();
    let label = display_name(&account);

    let outcome = api::checkin_account(&account).await;
    let ok = outcome.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let result = outcome
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_string();
    let message = outcome
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if let Some(patch) = outcome.get("patch") {
        let _ = store::apply_patch(&id, patch);
    }
    store::record_checkin(&id, &result, &message);
    logs::append(&id, &label, &result, &message);

    json!({
        "id": id,
        "label": label,
        "ok": ok,
        "result": result,
        "message": message,
    })
}

/// 跑一轮：所有账号依次签到。`app` 存在时逐条推送进度。
async fn run_cycle(app: Option<tauri::AppHandle>) -> Value {
    let Some(_guard) = CycleGuard::acquire() else {
        return json!({
            "total": 0, "succeeded": 0, "failed": 0, "items": [],
            "skipped": true, "finishedAt": paths::now_ms(),
        });
    };

    let accounts = store::load_accounts();
    let total = accounts.len();
    let mut items = Vec::with_capacity(total);
    let mut succeeded = 0usize;

    for (index, account) in accounts.into_iter().enumerate() {
        let item = perform(account).await;
        if item.get("ok").and_then(Value::as_bool) == Some(true) {
            succeeded += 1;
        }
        if let Some(handle) = app.as_ref() {
            let _ = handle.emit(
                "checkin://progress",
                json!({ "index": index + 1, "total": total, "item": item }),
            );
        }
        items.push(item);
    }

    let summary = json!({
        "total": total,
        "succeeded": succeeded,
        "failed": total.saturating_sub(succeeded),
        "items": items,
        "finishedAt": paths::now_ms(),
    });

    if let Some(handle) = app.as_ref() {
        let _ = handle.emit("checkin://finished", summary.clone());
    }
    summary
}

/// 条件满足时跑一轮；开关关闭或没有账号时直接跳过。
async fn run_scheduled(app: &tauri::AppHandle) {
    let config = paths::load_config();
    if config.auto_enabled && !store::load_accounts().is_empty() {
        let _ = run_cycle(Some(app.clone())).await;
    }
}

/// 后台调度：启动即签 + 每日兜底。
///
/// 签到一天只有一次机会，所以不做高频轮询：启动后先跑一轮覆盖当天，此后每天到达
/// 兜底时刻再跑一轮。兜底判定用墙钟比较而非「睡到目标时刻」的定时器，因此系统睡眠
/// 唤醒后也能正确补上；30 秒的检查粒度同时让配置改动（开关、时刻）半分钟内生效。
fn spawn_scheduler(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        // 给窗口和首屏留出加载时间，避免启动瞬间与界面渲染抢资源
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;

        // 第一轮：启动即签
        run_scheduled(&app).await;

        // 启动时若今天的兜底时刻已过，当天的名额已被这次启动覆盖，不再补跑；
        // 若还没到则留空，等时刻到了再跑一次，这样「启动」与「兜底」各算一次。
        let config = paths::load_config();
        let mut last_backup_day = if paths::today_reached(config.backup_hour, config.backup_minute) {
            paths::today_local()
        } else {
            String::new()
        };

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;

            let config = paths::load_config();
            let today = paths::today_local();
            if paths::today_reached(config.backup_hour, config.backup_minute)
                && today != last_backup_day
            {
                run_scheduled(&app).await;
                last_backup_day = today;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

#[tauri::command]
fn list_accounts() -> Vec<Value> {
    store::view_all()
}

#[tauri::command]
fn store_paths() -> Value {
    json!({
        "dataDir": paths::data_dir().to_string_lossy(),
        "accounts": paths::accounts_file().to_string_lossy(),
        "logs": paths::logs_file().to_string_lossy(),
        "config": paths::config_file().to_string_lossy(),
    })
}

#[tauri::command]
fn add_account(
    uid: Option<String>,
    nickname: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    domain: Option<String>,
) -> Result<String, String> {
    store::add_manual(uid, nickname, access_token, refresh_token, domain)
}

#[tauri::command]
fn import_local_accounts() -> Value {
    store::import_local()
}

#[tauri::command]
fn delete_account(id: String) -> Result<(), String> {
    store::remove(&id)
}

#[tauri::command]
fn get_config() -> paths::AppConfig {
    paths::load_config()
}

#[tauri::command]
fn set_config(
    auto_enabled: bool,
    backup_hour: u32,
    backup_minute: u32,
) -> Result<paths::AppConfig, String> {
    // 先读后改：配置里还有本地 AI 监控等本命令不负责的字段，
    // 直接构造新结构会把它们重置成默认值。
    let mut config = paths::load_config();
    config.auto_enabled = auto_enabled;
    config.backup_hour = backup_hour;
    config.backup_minute = backup_minute;
    let config = config.normalized();
    paths::save_config(&config)?;
    Ok(config)
}

#[tauri::command]
fn get_logs(limit: Option<usize>) -> Vec<Value> {
    logs::list(limit.unwrap_or(80).min(500))
}

/// 即时采一次硬盘与内存，供面板首帧填充。
///
/// 网速与 CPU 要两次采样求差，这里给 `null`，由 `sysmon` 的常驻线程每秒补上。
#[tauri::command]
fn sys_metrics() -> Value {
    sysmon::snapshot()
}

/// 改菜单栏显示项，立即生效。
///
/// 勾选写进配置（重启后保留），同时更新 `sysmon` 里的位图，下一次采样就按新
/// 勾选拼标题 —— 不用重启，也不用等配置重新加载。
#[tauri::command]
fn set_menubar_items(items: Vec<String>) -> Result<paths::AppConfig, String> {
    let mut config = paths::load_config();
    config.menubar_items = sysmon::normalize(&items);
    let config = config.normalized();
    paths::save_config(&config)?;
    sysmon::set_items(&config.menubar_items);
    Ok(config)
}

#[tauri::command]
async fn checkin_one(id: String) -> Value {
    let Some(_guard) = CycleGuard::acquire() else {
        return json!({
            "ok": false,
            "result": "error",
            "message": "已有签到任务在进行中，请稍候再试",
        });
    };

    let Some(account) = store::load_accounts()
        .into_iter()
        .find(|account| store::get_str(account, "id").as_deref() == Some(id.as_str()))
    else {
        return json!({ "ok": false, "result": "error", "message": "账号不存在" });
    };
    perform(account).await
}

#[tauri::command]
async fn checkin_all(app: tauri::AppHandle) -> Value {
    run_cycle(Some(app)).await
}

// ---------------------------------------------------------------------------
// 独立授权登录
//
// 与「从本机导入」的区别：不读官方客户端的登录态文件，每个账号各自走一次官方
// 授权，所以不受「客户端同一时刻只能登录一个账号」的限制。
// ---------------------------------------------------------------------------

#[tauri::command]
async fn oauth_login_start() -> Result<Value, String> {
    oauth::start().await
}

#[tauri::command]
fn oauth_login_open(login_id: String) -> Result<(), String> {
    oauth::open_login_page(&login_id)
}

#[tauri::command]
async fn oauth_login_poll(login_id: String) -> Value {
    oauth::poll(&login_id).await
}

#[tauri::command]
fn oauth_login_cancel(login_id: String) -> bool {
    oauth::cancel(&login_id)
}

// ---------------------------------------------------------------------------
// 积分查询
//
// 只读接口：查余额不会消耗积分，也不影响签到。
// ---------------------------------------------------------------------------

/// 把积分查询结果整理成一条前端记录，并顺带落库刷新后的 token。
fn credits_item(account: &Value, outcome: &Value) -> Value {
    let id = store::get_str(account, "id").unwrap_or_default();

    // patch 里是新 token，属于账号库内部数据，不能随结果交给前端。
    if let Some(patch) = outcome.get("patch").filter(|value| !value.is_null()) {
        let _ = store::apply_patch(&id, patch);
    }

    let mut item = json!({ "id": id, "label": display_name(account) });
    if let (Some(target), Some(source)) = (item.as_object_mut(), outcome.as_object()) {
        for (key, value) in source {
            if key == "patch" {
                continue;
            }
            target.insert(key.clone(), value.clone());
        }
    }
    item
}

#[tauri::command]
async fn fetch_credits_one(id: String) -> Value {
    let Some(account) = store::load_accounts()
        .into_iter()
        .find(|account| store::get_str(account, "id").as_deref() == Some(id.as_str()))
    else {
        return json!({ "ok": false, "message": "账号不存在" });
    };

    credits_item(&account, &credits::fetch(&account).await)
}

/// 查所有账号的积分。串行执行：账号数量少，比起并发更在意不把网关打疼。
#[tauri::command]
async fn fetch_credits_all() -> Value {
    let accounts = store::load_accounts();
    let mut items = Vec::with_capacity(accounts.len());

    for account in &accounts {
        let outcome = credits::fetch(account).await;
        items.push(credits_item(account, &outcome));
    }

    json!({
        "total": accounts.len(),
        "items": items,
        "finishedAt": paths::now_ms(),
    })
}

// ---------------------------------------------------------------------------
// 账号切换
//
// 会覆写官方客户端的登录态；restart 为真时还会关闭并重启客户端。
// 界面必须在执行前把这些后果讲清楚。
// ---------------------------------------------------------------------------

#[tauri::command]
fn switch_preflight() -> Value {
    switch::preflight()
}

#[tauri::command]
async fn switch_to_account(
    id: String,
    restart: bool,
    keep_session: bool,
    credits_only: Option<bool>,
) -> Result<Value, String> {
    switch::switch_account(&id, restart, keep_session, credits_only.unwrap_or(false)).await
}

// ---------------------------------------------------------------------------
// 分层快照
//
// 一个账号一套现场（L1 登录态 / L2 用户目录 / L3 Chromium 会话 / L4 侧边栏与
// 当前会话路由）。切换时成套换回去，会话才不会因为换号而消失。
// ---------------------------------------------------------------------------

#[tauri::command]
fn profile_list() -> Value {
    profile::list()
}

/// 保存"当前登录账号"的现场。
///
/// 客户端在跑时 session 目录仍在被写，直接拷会拿到半份 Cookies。这里默认
/// `auto_close`：先优雅退出客户端，存完再拉起来——否则用户得先手动 ⌘Q，
/// 而按钮就在客户端旁边，这一步几乎必然被忘掉。
#[tauri::command]
async fn profile_save(auto_close: Option<bool>) -> Result<Value, String> {
    let auto = auto_close.unwrap_or(true);
    let mut closed = false;

    if switch::client_running() {
        if !auto {
            return Err("WorkBuddy 正在运行，请先退出客户端再保存现场".to_string());
        }
        switch::close_client(20)?;
        closed = true;
    }

    let uid = profile::current_uid().ok_or_else(|| "读不到当前登录账号".to_string())?;
    let saved = profile::save(&uid, &profile::label_of(&uid));

    // 存完（无论成败）都把客户端拉回来：失败时用户至少还在客户端里，
    // 不至于对着一个被关掉的窗口发呆。
    if closed {
        let _ = switch::launch_client();
    }

    let mut meta = saved?;
    if let Some(object) = meta.as_object_mut() {
        object.insert("restarted".to_string(), Value::Bool(closed));
    }
    Ok(meta)
}

#[tauri::command]
fn profile_delete(uid: String) -> Result<(), String> {
    profile::delete(&uid)
}

/// 当前「谁在登录」与「谁在付费」。
///
/// 直接读官方登录态文件：`account.uid` 是登录身份，accessToken 的 JWT `sub` 是付费账号。
/// 用「只换额度」切过之后两者不同，界面需要分开显示。
#[tauri::command]
fn current_identity() -> Value {
    switch::identity()
}

// ---------------------------------------------------------------------------
// 成长中心（旅行 / 盲盒 / 任务领奖）
//
// 三个动作各自独立容错，单项失败不影响其余项。遇凭证失效刷新一次后重跑。
// ---------------------------------------------------------------------------

fn growth_item(account: &Value, outcome: &Value) -> Value {
    let id = store::get_str(account, "id").unwrap_or_default();
    let label = display_name(account);
    if let Some(patch) = outcome.get("patch") {
        let _ = store::apply_patch(&id, patch);
    }
    let ok = outcome.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let message = ["travel", "lottery", "tasks"]
        .iter()
        .filter_map(|key| {
            outcome
                .get(*key)
                .and_then(|item| item.get("message"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>()
        .join("；");
    json!({
        "id": id,
        "label": label,
        "ok": ok,
        "message": message,
        "travel": outcome.get("travel").cloned().unwrap_or(Value::Null),
        "lottery": outcome.get("lottery").cloned().unwrap_or(Value::Null),
        "tasks": outcome.get("tasks").cloned().unwrap_or(Value::Null),
    })
}

#[tauri::command]
async fn growth_run_one(id: String) -> Value {
    let Some(account) = store::load_accounts()
        .into_iter()
        .find(|account| store::get_str(account, "id").as_deref() == Some(id.as_str()))
    else {
        return json!({ "ok": false, "message": "账号不存在" });
    };
    growth_item(&account, &growth::run(&account).await)
}

/// 所有账号依次跑一轮。账号之间留 1 秒间隔，避免把上游打急。
#[tauri::command]
async fn growth_run_all() -> Value {
    let accounts = store::load_accounts();
    let mut items = Vec::with_capacity(accounts.len());
    let mut succeeded = 0usize;

    for (index, account) in accounts.iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let outcome = growth::run(account).await;
        let item = growth_item(account, &outcome);
        if item.get("ok").and_then(Value::as_bool) == Some(true) {
            succeeded += 1;
        }
        items.push(item);
    }

    json!({
        "total": accounts.len(),
        "succeeded": succeeded,
        "failed": accounts.len().saturating_sub(succeeded),
        "items": items,
        "finishedAt": paths::now_ms(),
    })
}

// ---------------------------------------------------------------------------
// 积分趋势
//
// 官方 `get-user-request-usage` 直接返回最近 30 天的逐请求明细，日汇总在本地做。
// 每次成功查询都落一份缓存，界面首屏可以先用缓存渲染，不必等网络。
// ---------------------------------------------------------------------------

fn usage_cache_file() -> std::path::PathBuf {
    paths::data_dir().join("usage-cache.json")
}

fn save_usage_cache(payload: &Value) {
    if let Ok(text) = serde_json::to_string(payload) {
        let _ = paths::write_private(&usage_cache_file(), &text);
    }
}

#[tauri::command]
fn usage_cached() -> Value {
    std::fs::read_to_string(usage_cache_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({ "accounts": [], "days": [], "cached": true }))
}

#[tauri::command]
async fn fetch_usage_all() -> Value {
    let accounts = store::load_accounts();
    let mut items = Vec::with_capacity(accounts.len());
    let mut aggregated: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();

    for account in &accounts {
        let outcome = credits::fetch_usage(account).await;
        if let (Some(patch), Some(id)) = (
            outcome.get("patch"),
            store::get_str(account, "id"),
        ) {
            let _ = store::apply_patch(&id, patch);
        }
        if outcome.get("ok").and_then(Value::as_bool) == Some(true) {
            if let Some(days) = outcome.get("days").and_then(Value::as_array) {
                for day in days {
                    if let (Some(date), Some(credit)) = (
                        day.get("date").and_then(Value::as_str),
                        day.get("credit").and_then(Value::as_f64),
                    ) {
                        *aggregated.entry(date.to_string()).or_insert(0.0) += credit;
                    }
                }
            }
        }
        items.push(json!({
            "id": store::get_str(account, "id").unwrap_or_default(),
            "label": display_name(account),
            "ok": outcome.get("ok").cloned().unwrap_or(json!(false)),
            "message": outcome.get("message").cloned().unwrap_or(Value::Null),
            "today": outcome.get("today").cloned().unwrap_or(json!(0.0)),
            "week": outcome.get("week").cloned().unwrap_or(json!(0.0)),
            "month": outcome.get("month").cloned().unwrap_or(json!(0.0)),
            "requests": outcome.get("requests").cloned().unwrap_or(json!(0)),
            "days": outcome.get("days").cloned().unwrap_or(json!([])),
        }));
    }

    let days: Vec<Value> = aggregated
        .into_iter()
        .map(|(date, credit)| json!({ "date": date, "credit": (credit * 100.0).round() / 100.0 }))
        .collect();

    let payload = json!({
        "accounts": items,
        "days": days,
        "finishedAt": paths::now_ms(),
    });
    save_usage_cache(&payload);
    payload
}

// ---------------------------------------------------------------------------
// 本地 AI 监控（可选功能）
//
// 只读本机的推理后端与日志，不修改任何东西。功能默认关闭；关闭时后端不做
// 任何探测，不建连接、不读文件、不起进程。未部署本地 AI 的机器上打开也不会
// 报错，只会如实显示「没有服务在听」。
// ---------------------------------------------------------------------------

#[tauri::command]
async fn local_ai_status() -> Value {
    localai::status().await
}

#[tauri::command]
async fn local_ai_detect() -> Value {
    localai::detect().await
}

#[tauri::command]
fn local_ai_tail(file: String, lines: Option<usize>) -> Value {
    let config = paths::load_config().local_ai;
    localai::tail_log(&localai::log_dir(&config), &file, lines.unwrap_or(150))
}

#[tauri::command]
fn set_local_ai_config(
    enabled: bool,
    log_dir: String,
    smctemp_path: String,
) -> Result<paths::AppConfig, String> {
    localai::update_config(enabled, log_dir, smctemp_path)
}

// ---------------------------------------------------------------------------
// 模型摆渡
//
// 这里是本应用唯一会开本地端口的模块：转发口只绑 127.0.0.1，口令一次一发，
// 任何带 Origin 的请求一律拒绝。管理动作全部走 IPC，前端不直接访问 HTTP。
// ---------------------------------------------------------------------------

#[tauri::command]
async fn ferry_status() -> Value {
    ferry::status().await
}

#[tauri::command]
async fn ferry_start() -> Result<Value, String> {
    ferry::start().await
}

#[tauri::command]
async fn ferry_stop() -> Result<Value, String> {
    ferry::stop().await
}

#[tauri::command]
async fn ferry_probe(model: Option<String>) -> Result<Value, String> {
    ferry::probe(model).await
}

#[tauri::command]
async fn ferry_refresh() -> Result<Value, String> {
    ferry::refresh().await
}

#[tauri::command]
async fn ferry_import() -> Result<Value, String> {
    ferry::import_models().await
}

#[tauri::command]
async fn ferry_selection(model: String, selected: bool) -> Result<Value, String> {
    ferry::selection(model, selected).await
}

#[tauri::command]
async fn ferry_set_proxy(enabled: bool) -> Result<Value, String> {
    ferry::set_proxy(enabled).await
}

#[tauri::command]
fn ferry_logs(lines: Option<usize>) -> Value {
    ferry::logs(lines.unwrap_or(120))
}

// ---------------------------------------------------------------------------
// 菜单栏常驻
//
// 点窗口关闭按钮不退出进程，只把窗口收起来，签到调度继续在后台跑——这是
// 「自动签到」能按时触发的前提：进程一旦退出，兜底时刻就没人守了。
// 要真正退出，走菜单栏图标里的「退出 WB 签到」，或 ⌘Q。
// ---------------------------------------------------------------------------

/// 把主窗口显示出来并置于前台。
fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

// ---------------------------------------------------------------------------
// 菜单栏弹出面板
// ---------------------------------------------------------------------------

const POPOVER_LABEL: &str = "popover";

/// 面板宽度（逻辑点）固定；高度由前端量完内容回传，见 `popover_resize`。
const POPOVER_WIDTH: f64 = 352.0;

/// 面板初始高度，只在首帧测量回传之前顶一下。
const POPOVER_HEIGHT: f64 = 420.0;

/// 面板与屏幕边缘的留白。
const POPOVER_MARGIN: f64 = 8.0;

/// 面板每次弹出时发给前端的信号，用于触发数据刷新。
const POPOVER_SHOWN: &str = "popover-shown";

/// 面板收起时发给前端的信号，用于让它停下来（前端据此暂停轮询）。
const POPOVER_HIDDEN: &str = "popover-hidden";

/// 最近一次面板失焦的时刻（毫秒），用来消除「点不掉」。
///
/// 面板可见时点托盘图标，系统会**先让面板失焦、再派发托盘点击事件**。
/// 不记这个时刻的话，toggle 会把「用户想关掉」误判成「用户想打开」，
/// 表现为面板闪一下又回来、怎么点都关不掉。250ms 内的失焦归因于本次点击。
static LAST_POPOVER_BLUR_MS: AtomicU64 = AtomicU64::new(0);

/// 最近一次弹出面板的时刻（毫秒）。
///
/// 窗口从 `show` 到真正拿到焦点之间，系统可能先送一个失焦事件 ——
/// app 原本不在前台时尤其明显（实测在启动阶段弹出的面板会被自己立刻收掉）。
/// 不设这道闸，用户看到的就是「点开一闪就没」。300ms 内的失焦一律算作激活抖动。
static LAST_POPOVER_SHOW_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 取面板窗口，没有就建一个（建好即隐藏）。
///
/// 无边框 + 透明 + 置顶，接近系统面板观感；透明依赖 `macos-private-api`。
/// 之所以在启动阶段就预创建，是因为 webview 需要时间加载 —— 等首次点击才建，
/// 用户会看到空白面板一闪。预创建的窗口一直后台渲染，点开即显示。
fn popover_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    if let Some(win) = app.get_webview_window(POPOVER_LABEL) {
        return Ok(win);
    }
    WebviewWindowBuilder::new(app, POPOVER_LABEL, WebviewUrl::App("index.html".into()))
        .title("WB 签到")
        .inner_size(POPOVER_WIDTH, POPOVER_HEIGHT)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .resizable(false)
        .shadow(true)
        .skip_taskbar(true)
        .visible(false)
        .build()
}

/// 把面板摆到托盘图标正下方。
///
/// y 取自 `Monitor::work_area()`（菜单栏下沿），**刻意不用**托盘事件里的
/// `rect` / `position` —— 那两处在 `tray-icon` 内部是拿 `CGDisplayPixelsHigh`
/// 的结果当逻辑点去翻转 Retina 屏上的 y，算出来会掉到屏幕外。
/// `work_area` 是权威值，且天然适配刘海屏与外接屏。
/// x 用事件里的鼠标横坐标对齐图标中心：该字段未经那处翻转，可信。
fn place_popover(win: &WebviewWindow, cursor_x_physical: Option<f64>) -> tauri::Result<()> {
    let Some(monitor) = win.primary_monitor()? else {
        return Ok(());
    };
    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    let left = area.position.x as f64 / scale;
    let right = (area.position.x as f64 + area.size.width as f64) / scale;

    let desired = match cursor_x_physical {
        Some(x) => x / scale - POPOVER_WIDTH / 2.0,
        None => right - POPOVER_WIDTH - POPOVER_MARGIN,
    };
    // 夹在可用区内，免得图标贴近屏幕边缘时面板被切掉
    let min_x = left + POPOVER_MARGIN;
    let max_x = (right - POPOVER_WIDTH - POPOVER_MARGIN).max(min_x);

    // 纵向锚在菜单栏下沿再留一点空，免得贴着菜单栏显得挤
    win.set_position(LogicalPosition::new(
        desired.clamp(min_x, max_x),
        area.position.y as f64 / scale + POPOVER_MARGIN / 2.0,
    ))
}

/// 收起面板，并告诉前端「已经收起了」。
///
/// 前端靠这个信号决定要不要继续轮询：窗口是常驻的，没人看的时候还每 5 秒去探一次
/// 端口、读一遍温度纯属白耗电。窗口失焦事件也能起到同样作用，但那属于系统行为，
/// 由后端明确告知一声，就不必赌某条系统事件一定会来。
fn hide_popover(app: &AppHandle, win: &WebviewWindow) {
    let _ = win.hide();
    let _ = app.emit(POPOVER_HIDDEN, ());
}

/// 托盘左键：面板开着就收起，关着就弹出。
fn toggle_popover(app: &AppHandle, cursor_x_physical: Option<f64>) {
    let Ok(win) = popover_window(app) else {
        return;
    };
    if win.is_visible().unwrap_or(false) {
        hide_popover(app, &win);
        return;
    }
    // 刚因这次点击而失焦收起，说明用户要的是「关掉」，别再弹回来
    if now_ms().saturating_sub(LAST_POPOVER_BLUR_MS.load(Ordering::SeqCst)) < 250 {
        return;
    }
    if place_popover(&win, cursor_x_physical).is_err() {
        return;
    }
    LAST_POPOVER_SHOW_MS.store(now_ms(), Ordering::SeqCst);
    let _ = win.show();
    let _ = win.set_focus();
    // 面板是常驻窗口，显隐不会让它重新挂载，所以由后端发信号叫它刷新。
    // 用 app 级（EventTarget::Any）而不是 win 级：前端 listen 默认注册的目标也是
    // Any，这条链路与应用内其它事件（checkin://finished）一致，省得卡在目标匹配上。
    let _ = app.emit(POPOVER_SHOWN, ());
}

/// 收起面板（面板内按钮用）。
#[tauri::command]
fn popover_hide(app: AppHandle) {
    if let Some(win) = app.get_webview_window(POPOVER_LABEL) {
        hide_popover(&app, &win);
    }
}

/// 收起面板并唤出主窗口。
#[tauri::command]
fn popover_open_main(app: AppHandle) {
    popover_hide(app.clone());
    show_main_window(&app);
}

/// 真正退出进程。面板里的「退出」入口，等价于菜单栏那个。
#[tauri::command]
fn quit_app(app: AppHandle) {
    // app.exit 不会触发 ExitRequested，退出钩子在这里不会跑；
    // 不显式停一次，sidecar 会被过继给 launchd 继续占着 41980。
    ferry::stop_blocking();
    app.exit(0);
}

/// 面板高度贴合内容。
///
/// 固定高度在内容少时会留一大块空白（AI 监控关掉的那台机器尤其明显），
/// 所以由前端量完实际高度回传。宽度不动 —— 面板是贴着图标摆的，宽度跳会显得晃。
#[tauri::command]
fn popover_resize(app: AppHandle, height: f64) {
    let Some(win) = app.get_webview_window(POPOVER_LABEL) else {
        return;
    };
    // 上限取屏幕可用高度，免得内容太长时面板伸出屏幕外
    let cap = win
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| m.work_area().size.height as f64 / m.scale_factor() - 40.0)
        .unwrap_or(900.0);
    let h = height.clamp(220.0, cap.max(220.0));
    let _ = win.set_size(LogicalSize::new(POPOVER_WIDTH, h));
}

/// 建系统托盘（macOS 上是右上角菜单栏图标）。
///
/// 一共两个图标：主图标放硬盘/CPU/内存，另一个只放网速。
/// 网速要占两行，和硬盘挤在同一个图标里会把硬盘百分比挤到第二行去，
/// 拆开之后两边各占一个图标位，互不影响。
///
/// 左键弹自定义面板；原生菜单只留给右键 —— 面板万一没出来，右键仍能进主窗口
/// 或退出，不至于卡死在一个没有入口的后台进程里。图标以模板图模式加载，
/// 由系统按菜单栏深浅色自动反色。
fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    // 先建网速、后建主图标：macOS 把新的状态栏项插在左边，
    // 这样排出来才是「硬盘/CPU/内存 在左，网速在右」。
    // 网速这个只显示文字，不给图标 —— 两个一模一样的图标并排反而难认。
    TrayIconBuilder::with_id(NET_TRAY_ID)
        .tooltip("实时网速")
        .menu(&build_tray_menu(app)?)
        .show_menu_on_left_click(false)
        .on_menu_event(tray_menu_event)
        .on_tray_icon_event(tray_icon_event)
        .build(app)?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tauri::include_image!("icons/tray.png"))
        .icon_as_template(true)
        .tooltip("WB 签到 · 后台运行中")
        .menu(&build_tray_menu(app)?)
        .show_menu_on_left_click(false)
        .on_menu_event(tray_menu_event)
        .on_tray_icon_event(tray_icon_event)
        .build(app)?;

    Ok(())
}

/// 两个图标共用同一套菜单项（各自建一份，macOS 的 NSMenu 只能挂在一个状态栏项上）。
fn build_tray_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let show_item = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let checkin_item = MenuItem::with_id(app, "checkin", "立即签到全部账号", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出 WB 签到", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;

    Menu::with_items(app, &[&show_item, &checkin_item, &separator, &quit_item])
}

fn tray_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id.as_ref() {
        "show" => show_main_window(app),
        "checkin" => {
            // 菜单回调是同步的，签到要发网络请求，丢到异步运行时里跑
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let _ = run_cycle(Some(handle)).await;
            });
        }
        "quit" => {
            ferry::stop_blocking();
            app.exit(0);
        }
        _ => {}
    }
}

fn tray_icon_event(tray: &TrayIcon, event: TrayIconEvent) {
    // 只在「左键抬起」时切换，避免按下、抬起各触发一次
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        position,
        ..
    } = event
    {
        // position.x 用来把面板对齐到图标中心（见 place_popover 的说明）
        toggle_popover(tray.app_handle(), Some(position.x));
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let _ = paths::ensure_private_dir(&paths::data_dir());
            build_tray(app.handle())?;
            // 菜单栏指标要跟着托盘一起活：窗口收起后仍要每秒刷新。
            sysmon::spawn(app.handle().clone());

            // 面板预先建好并隐藏：webview 提前加载完，首次点击才能秒开
            let _ = popover_window(app.handle());
            spawn_scheduler(app.handle().clone());
            // 上次退出前主动开过「模型摆渡」才自动拉起；默认关闭，不占端口。
            ferry::autostart_on_launch();
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // 关闭按钮 = 收起窗口，不退出进程。
            // 进程常驻是自动签到能按时触发的前提，退出入口只保留在菜单栏。
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            // 面板失焦即收起：点面板以外的任何地方都该把它关掉。
            // 顺手记下时刻给 toggle_popover 去重，否则点托盘关不掉面板。
            WindowEvent::Focused(false) if window.label() == POPOVER_LABEL => {
                let now = now_ms();
                // 刚弹出时的失焦是激活过程抖动，不代表用户点了外面，别理它
                if now.saturating_sub(LAST_POPOVER_SHOW_MS.load(Ordering::SeqCst)) < 300 {
                    return;
                }
                LAST_POPOVER_BLUR_MS.store(now, Ordering::SeqCst);
                let _ = window.hide();
                let _ = window.app_handle().emit(POPOVER_HIDDEN, ());
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            list_accounts,
            store_paths,
            add_account,
            import_local_accounts,
            delete_account,
            get_config,
            set_config,
            get_logs,
            checkin_one,
            checkin_all,
            oauth_login_start,
            oauth_login_open,
            oauth_login_poll,
            oauth_login_cancel,
            fetch_credits_one,
            fetch_credits_all,
            switch_preflight,
            switch_to_account,
            profile_list,
            profile_save,
            profile_delete,
            current_identity,
            growth_run_one,
            growth_run_all,
            usage_cached,
            fetch_usage_all,
            local_ai_status,
            local_ai_detect,
            local_ai_tail,
            set_local_ai_config,
            sys_metrics,
            set_menubar_items,
            ferry_status,
            ferry_start,
            ferry_stop,
            ferry_probe,
            ferry_refresh,
            ferry_import,
            ferry_selection,
            ferry_set_proxy,
            ferry_logs,
            popover_hide,
            popover_open_main,
            popover_resize,
            quit_app,
        ])
        .build(tauri::generate_context!())
        .expect("启动应用失败");

    app.run(|handle, event| {
        // 退出前把摆渡子进程收掉：否则它会变成孤儿进程继续占着 41980。
        //
        // 两个事件都要接：`ExitRequested` 只在"关窗 / 显式 exit"时来，
        // 而 macOS 的 ⟩退出 / AppleScript `quit` 走 NSApp terminate，只发 `Exit`。
        // 实测只接前者时，退出后子进程仍活着并握着端口。`stop_blocking` 是幂等的。
        match event {
            tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit => ferry::stop_blocking(),
            _ => {}
        }

        // 窗口收起后点 Dock 图标，系统发 Reopen，把主窗口重新调出来
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            show_main_window(handle);
        }

        #[cfg(not(target_os = "macos"))]
        let _ = (handle, event);
    });
}
