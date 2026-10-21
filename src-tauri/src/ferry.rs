//! 模型摆渡（OC Ferry）守护层。
//!
//! 定位：把 OpenCode Zen 的免费模型摆渡给 WorkBuddy。真正干活的是一个内置的
//! Node sidecar（`sidecar/src/main.js`），它自己起一个隔离的 `opencode serve`，
//! 并在 `127.0.0.1:41980` 上暴露 OpenAI 兼容的转发口。本模块只做三件事：
//!
//! 1. 找到 Node 运行时与 sidecar，按私有目录拉起子进程；
//! 2. 探活、崩溃后退避重启（子进程内部也会自我恢复，这里兜的是进程级退出）；
//! 3. 把 sidecar 的管理接口转成 IPC 命令，前端不直接碰 HTTP。
//!
//! 安全口径：转发口只绑回环、每次启动重新生成 Bearer、拒绝任何带 `Origin` 的请求
//! （见 sidecar/src/server.js）。这里是本应用唯一一处会开本地端口的功能，
//! 因此开关默认关闭，且只能由用户显式启动。

use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::paths;

const PORT: u16 = 41980;

/// 子进程句柄。sidecar 是唯一的写入者，主进程只负责生死。
static CHILD: Mutex<Option<Child>> = Mutex::new(None);
/// 拉起过程是异步的：挡掉并发的第二次启动，否则会撞端口。
static STARTING: Mutex<bool> = Mutex::new(false);

pub fn data_dir() -> PathBuf {
    paths::data_dir().join("ferry")
}

fn enabled_marker() -> PathBuf {
    data_dir().join("enabled")
}

fn log_file() -> PathBuf {
    data_dir().join("ferry.log")
}

fn api_key_file() -> PathBuf {
    data_dir().join("api-key")
}

fn endpoint() -> String {
    format!("http://127.0.0.1:{PORT}")
}

/// sidecar 位置：打包后在 `Resources/sidecar`，开发态在仓库的 `../sidecar`。
fn sidecar_dir() -> Result<PathBuf, String> {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("../Resources/sidecar")))
        .filter(|dir| dir.join("src/main.js").exists());
    if let Some(dir) = bundled {
        return Ok(dir);
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sidecar");
    if dev.join("src/main.js").exists() {
        return Ok(dev);
    }
    Err("找不到内置的摆渡程序（sidecar），请重新安装应用".into())
}

/// Node 运行时。sidecar 需要 Node 22+。
///
/// 结果缓存：状态面板每 2.5 秒读一次状态，每读一次都去 fork 两个进程探版本是不可接受的。
pub fn resolve_node() -> Result<PathBuf, String> {
    static CACHE: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(discover_node).clone()
}

/// 查找顺序刻意写成固定候选表而不是依赖 PATH：从 Dock 或 Finder 启动的 .app
/// 拿不到登录 shell 的 PATH，`/opt/homebrew/bin` 通常不在里面。
fn discover_node() -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(explicit) = std::env::var("WB_FERRY_NODE") {
        candidates.push(PathBuf::from(explicit));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../Resources/node/bin/node"));
        }
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/node"));
    candidates.push(PathBuf::from("/usr/local/bin/node"));
    candidates.push(PathBuf::from("/usr/bin/node"));
    if let Ok(output) = Command::new("/bin/sh")
        .args(["-lc", "command -v node"])
        .output()
    {
        let found = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !found.is_empty() {
            candidates.push(PathBuf::from(found));
        }
    }
    // WorkBuddy 自带的运行时：版本号目录名是定宽的，倒序取最新的一个。
    if let Ok(entries) = fs::read_dir(paths::home_dir().join(".workbuddy/binaries/node/versions")) {
        let mut versions: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path().join("bin/node"))
            .filter(|path| path.exists())
            .collect();
        versions.sort();
        versions.reverse();
        candidates.extend(versions);
    }

    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        if let Ok(output) = Command::new(&candidate).arg("--version").output() {
            if output.status.success() {
                let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
                // v22.22.2 → 22
                let major: u32 = version
                    .trim_start_matches('v')
                    .split('.')
                    .next()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0);
                if major >= 22 {
                    return Ok(candidate);
                }
            }
        }
    }
    Err("未找到 Node.js 22+。模型摆渡需要一个 Node 运行时；装好后重试，或在环境变量 WB_FERRY_NODE 里指定路径。".into())
}

fn ensure_dir() -> Result<(), String> {
    let dir = data_dir();
    fs::create_dir_all(&dir).map_err(|error| format!("创建目录失败：{error}"))?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    Ok(())
}

/// 首次接管独立版 OC Ferry 的现场。
///
/// 最关键的是 `api-key`：`models.json` 里已导入的条目带着旧口令，换一把新口令就会
/// 401，用户会看到"模型突然全部失效"。沿用同一把口令，接管是无感的。
/// `settings.json` 里存的是用户手动关掉过的模型清单，也一并沿用。
#[cfg(target_os = "macos")]
fn migrate_legacy_state() {
    let legacy = paths::home_dir().join("Library/Application Support/OC Ferry");
    if !legacy.exists() {
        return;
    }
    for name in ["api-key", "settings.json"] {
        let target = data_dir().join(name);
        if target.exists() {
            continue;
        }
        let Ok(bytes) = fs::read(legacy.join(name)) else {
            continue;
        };
        if let Ok(mut file) = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&target)
        {
            let _ = file.write_all(&bytes);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn migrate_legacy_state() {}

fn child_running() -> bool {
    let mut guard = CHILD.lock().unwrap();
    match guard.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(_)) => {
                *guard = None;
                false
            }
            Ok(None) => true,
            Err(_) => false,
        },
        None => false,
    }
}

/// 占着 41980 的是不是我们自己的 sidecar，只是句柄丢了。
///
/// 场景：客户端被强退（崩溃、⌘Q、活动监视器），子进程被过继给 launchd（PPID 变 1）
/// 却仍在正常服务。新起的客户端手里没有 `Child`，于是「面板显示已停止 → 点启动 →
/// 报端口被占用 → 点停止又说不清」，形成死锁。
///
/// 判据只认一条：监听者 PID 与本目录 `service.pid` 记录一致。该文件在 0700 目录下、
/// 由 sidecar 自己以 0600 写入，别的程序写不进来 —— 所以它比"命令行里含 sidecar"更可靠。
fn orphan_pid() -> Option<i32> {
    let recorded: i32 = fs::read_to_string(data_dir().join("service.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let output = Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{PORT}"), "-sTCP:LISTEN", "-t"])
        .output()
        .ok()?;
    let listener: i32 = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()?;
    (listener == recorded).then_some(listener)
}

/// 端口是不是正被我们自己的 sidecar 服务着（含被过继的那种）。
fn own_sidecar_serving() -> bool {
    // 先做一次零成本判断：没人监听就绝不去 spawn lsof。
    std::net::TcpStream::connect(("127.0.0.1", PORT)).is_ok() && orphan_pid().is_some()
}

/// 让一个没有句柄的 sidecar 走正常退出路径（它收到 SIGTERM 会先摘掉 models.json 里的条目）。
fn terminate_orphan(pid: i32) {
    unsafe { libc::kill(pid, libc::SIGTERM) };
}

fn wait_port_free(limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", PORT)).is_err() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::net::TcpStream::connect(("127.0.0.1", PORT)).is_err()
}

fn api_key() -> Result<String, String> {
    fs::read_to_string(api_key_file())
        .map(|text| text.trim().to_string())
        .map_err(|_| "转发口还没生成访问密钥".to_string())
}

async fn call(method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
    let key = api_key()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| error.to_string())?;
    let mut request = client
        .request(method, format!("{}{}", endpoint(), path))
        .bearer_auth(key);
    if let Some(payload) = body {
        request = request.json(&payload);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("转发口无响应：{error}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "转发口返回 {status}：{}",
            text.chars().take(300).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|error| format!("响应不是 JSON：{error}"))
}

/// 等 sidecar 把自己和 opencode 都拉起来。首次运行要下载 opencode 运行时，
/// 所以预算给到 90 秒；期间前端能看到 phase 变化。
async fn wait_ready(seconds: u64) -> Result<Value, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    let mut last = String::from("等待服务就绪");
    while std::time::Instant::now() < deadline {
        if !child_running() {
            // 子进程自己写下的死因就在 status.json 里（phase=error 的 message），
            // 只回一句"进程已退出"等于让用户去翻一份什么都看不出来的日志。
            let reason = fs::read_to_string(data_dir().join("status.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| value.get("message").and_then(Value::as_str).map(str::to_string))
                .filter(|text| !text.is_empty());
            return Err(match reason {
                Some(text) => format!("摆渡进程已退出：{text}"),
                None => "摆渡进程已退出，请查看日志".into(),
            });
        }
        match call(reqwest::Method::GET, "/health", None).await {
            Ok(health) => {
                let phase = health
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if phase == "ready" || phase == "reading" {
                    return Ok(health);
                }
                last = health
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("启动中")
                    .to_string();
            }
            Err(error) => last = error,
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    Err(format!("启动超时：{last}"))
}

pub async fn start() -> Result<Value, String> {
    {
        let mut starting = STARTING.lock().unwrap();
        if *starting {
            return Err("正在启动中".into());
        }
        *starting = true;
    }
    let result = start_inner().await;
    *STARTING.lock().unwrap() = false;
    result
}

/// 不依赖子进程句柄的健康探测：用于"端口有人在服务，但句柄不在我们手里"的情形。
async fn probe_health(limit: Duration) -> Option<Value> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if let Ok(health) = call(reqwest::Method::GET, "/health", None).await {
            return Some(health);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn start_inner() -> Result<Value, String> {
    if child_running() {
        return wait_ready(5).await;
    }
    // 端口只有一个主人。先分清是"别人"还是"我们自己上一轮留下的"：
    // 独立版 OC Ferry / 任何第三方进程 → 如实报占用；
    // 我们自己的孤儿 → 直接接管，它本来就在正常服务，用户不该为此做任何操作。
    if std::net::TcpStream::connect(("127.0.0.1", PORT)).is_ok() {
        if let Some(pid) = orphan_pid() {
            match probe_health(Duration::from_secs(5)).await {
                // 同一版本：原样接管，省掉一次冷启动和三分钟的模型检测。
                Some(health) if health.get("stamp").and_then(Value::as_str) == Some(env!("CARGO_PKG_VERSION")) => {
                    let mut health = health;
                    health["adopted"] = json!(true);
                    health["adoptedPid"] = json!(pid);
                    let _ = fs::write(enabled_marker(), b"1");
                    return Ok(health);
                }
                // 上一版留下的实例：升级后必须换掉，否则新包装的补丁永远跑不到，
                // 表现为"改了却没生效"——这种问题比端口冲突难查得多。
                _ => {
                    terminate_orphan(pid);
                    wait_port_free(Duration::from_secs(5));
                }
            }
        }
        if std::net::TcpStream::connect(("127.0.0.1", PORT)).is_ok() {
            return Err(format!(
                "端口 {PORT} 已被占用。若独立版 OC Ferry 正在运行，请先从它的托盘退出，再在这里启动。"
            ));
        }
    }
    ensure_dir()?;
    migrate_legacy_state();
    let node = resolve_node()?;
    let sidecar = sidecar_dir()?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_file())
        .map_err(|error| format!("打开日志失败：{error}"))?;
    let log_err = log.try_clone().map_err(|error| error.to_string())?;

    let mut command = Command::new(&node);
    command
        .arg(sidecar.join("src/main.js"))
        .current_dir(&sidecar)
        .env("FERRY_DATA_DIR", data_dir())
        .env("FERRY_PORT", PORT.to_string())
        .env(
            "FERRY_MODELS_FILE",
            paths::home_dir().join(".workbuddy/models.json"),
        )
        // XDG 目录也指向私有目录。opencode 的 `--version` 探活会在启动前跑一次，
        // 那时还没有隔离配置；它要往 ~/.local/share 写东西——本机该目录属于 root，
        // 于是「版本探测失败 → 回落到旧运行时」或者干脆起不来。
        .env("XDG_DATA_HOME", data_dir().join("xdg/data"))
        .env("XDG_CONFIG_HOME", data_dir().join("xdg/config"))
        .env("XDG_CACHE_HOME", data_dir().join("xdg/cache"))
        .env("XDG_STATE_HOME", data_dir().join("xdg/state"))
        // 本机实测：并发 3、超时 60 秒时，一轮 12 个模型的检测要 3 分钟。
        // 并发提到 6、超时收到 30 秒，慢模型不再挡住快模型。
        .env("FERRY_PROBE_CONCURRENCY", "6")
        .env("FERRY_PROBE_TIMEOUT", "30000")
        // 纯文本请求边生成边下发。信封模式（带工具）默认仍是整段校验后交付，
        // 见 sidecar/src/protocol.js 里 EnvelopeStream 的说明。
        .env("FERRY_STREAM_CHAT", "1")
        // 让 sidecar 在 /health 里回这个版本号：接管孤儿进程前要靠它确认
        // "跑的是不是本版本该跑的代码"，而不是上一版留下的旧 JS。
        .env("FERRY_BUILD_STAMP", env!("CARGO_PKG_VERSION"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    let child = command
        .spawn()
        .map_err(|error| format!("启动失败：{error}"))?;
    *CHILD.lock().unwrap() = Some(child);
    // 让 sidecar 下次随应用一起起来。
    let _ = fs::write(enabled_marker(), b"1");

    match wait_ready(90).await {
        Ok(health) => Ok(health),
        Err(error) => {
            // 起不来就不要留一个半死的进程占着端口。
            let _ = stop().await;
            Err(error)
        }
    }
}

pub async fn stop() -> Result<Value, String> {
    let child = CHILD.lock().unwrap().take();
    let Some(mut child) = child else {
        // 句柄不在、端口却还被我们自己的 sidecar 占着：上一个客户端留下的孤儿，按 PID 停。
        let adopted = orphan_pid();
        if let Some(pid) = adopted {
            terminate_orphan(pid);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline
                && std::net::TcpStream::connect(("127.0.0.1", PORT)).is_ok()
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        let _ = fs::remove_file(enabled_marker());
        return Ok(json!({
            "running": false,
            "phase": "stopped",
            "message": if adopted.is_some() { "已停止（接管来的实例）" } else { "未在运行" },
        }));
    };
    // sidecar 收到 SIGTERM 会把自己导入 models.json 的条目摘掉再退出。
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    for _ in 0..50 {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(enabled_marker());
    Ok(json!({ "running": false, "phase": "stopped", "message": "已停止" }))
}

/// 退出钩子用的同步版本：事件循环退出时没有 tokio 运行时可以 await。
pub fn stop_blocking() {
    let child = CHILD.lock().unwrap().take();
    let Some(mut child) = child else {
        // 孤儿实例同样要带走，否则客户端退了、端口还占着，下次启动就撞车。
        if let Some(pid) = orphan_pid() {
            terminate_orphan(pid);
            wait_port_free(Duration::from_secs(5));
        }
        return;
    };
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    for _ in 0..40 {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub async fn status() -> Value {
    // 有句柄按句柄算；句柄丢了但端口正被我们自己的 sidecar 服务，也如实算"在跑"，
    // 否则面板会显示已停止，而用户点启动又被告知端口被占用。
    let running = child_running() || own_sidecar_serving();
    let mut base = json!({
        "running": running,
        "port": PORT,
        "endpoint": format!("{}/v1", endpoint()),
        "dataDir": data_dir().to_string_lossy(),
        "nodePath": resolve_node().map(|path| path.to_string_lossy().to_string()).unwrap_or_default(),
        "sidecarDir": sidecar_dir().map(|path| path.to_string_lossy().to_string()).unwrap_or_default(),
    });
    if !running {
        base["phase"] = json!("stopped");
        base["message"] = json!("未在运行");
        return base;
    }
    match call(reqwest::Method::GET, "/health", None).await {
        Ok(health) => {
            if let (Some(target), Some(source)) = (base.as_object_mut(), health.as_object()) {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
            base
        }
        Err(error) => {
            base["phase"] = json!("unreachable");
            base["message"] = json!(error);
            base
        }
    }
}

pub async fn probe(model: Option<String>) -> Result<Value, String> {
    call(
        reqwest::Method::POST,
        "/admin/probe",
        Some(json!({ "model": model })),
    )
    .await
}

pub async fn refresh() -> Result<Value, String> {
    call(reqwest::Method::POST, "/admin/refresh", Some(json!({}))).await
}

pub async fn import_models() -> Result<Value, String> {
    call(reqwest::Method::POST, "/admin/import", Some(json!({}))).await
}

pub async fn selection(model: String, selected: bool) -> Result<Value, String> {
    call(
        reqwest::Method::POST,
        "/admin/selection",
        Some(json!({ "model": model, "selected": selected })),
    )
    .await
}

pub async fn set_proxy(enabled: bool) -> Result<Value, String> {
    call(
        reqwest::Method::POST,
        "/admin/system-proxy",
        Some(json!({ "enabled": enabled })),
    )
    .await
}

/// 日志尾部：一行一条，前端只读不写。读不到就给空数组，不报错。
pub fn logs(lines: usize) -> Value {
    let mut out: Vec<String> = Vec::new();
    for path in [log_file(), data_dir().join("opencode.log")] {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let tail: Vec<&str> = text.lines().rev().take(lines).collect();
        out.extend(tail.into_iter().rev().map(str::to_string));
    }
    json!({ "lines": out, "file": log_file().to_string_lossy() })
}

/// 应用启动时的自动拉起：只有用户上次主动开过（`enabled` 标记还在）才启动。
pub fn autostart_on_launch() {
    if !enabled_marker().exists() {
        return;
    }
    tauri::async_runtime::spawn(async {
        if let Err(error) = start().await {
            // 自动拉起失败不应该影响签到主流程，只留一条日志。
            let _ = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(log_file())
                .and_then(|mut file| writeln!(file, "[autostart] {error}"));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_is_private() {
        ensure_dir().unwrap();
        let mode = fs::metadata(data_dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "摆渡数据目录必须是 0700");
    }

    #[test]
    fn sidecar_exists_in_dev() {
        let dir = sidecar_dir().expect("开发态应能找到 sidecar");
        assert!(dir.join("src/main.js").exists());
    }
}
