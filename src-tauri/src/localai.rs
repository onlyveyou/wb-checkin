//! 本地 AI 监控：探活 llama.cpp 推理后端与按需网关，读 SMC 温度，跟随运行日志。
//!
//! 移植自 `ai_status_dashboard.py`，但改成进程内实现：不再依赖 Python 解释器，
//! 也不再额外起一个常驻的本地 HTTP 服务——后者没有鉴权，同机任何进程或网页都能调用。
//!
//! 探活口径（2026-09-16 定案，勿改）：
//! - 按需网关**只做 TCP 探测，绝不发 HTTP**：任何请求都会把它唤醒，触发 9~29 秒冷启动；
//! - 文本 / 视觉后端直连 `/v1/models` 与 `/slots`；
//! - 三态 ready / cold（网关空闲，属设计内行为，不是故障）/ down。
//!
//! 未部署本地 AI 的机器上，本模块同样可用：探测会如实返回「没有服务在听」，
//! 不会报错。功能开关默认关闭，关闭时不做任何探测。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::paths::{self, LocalAiConfig, LocalAiService};

/// TCP 探测超时。loopback 上端口关闭会立刻返回 ECONNREFUSED，这个超时只用于兜底。
const TCP_TIMEOUT: Duration = Duration::from_millis(1000);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
/// smctemp 读 SMC 一般几十毫秒；个别机型上可能卡住，给个上限免得占死线程。
const SMCTEMP_TIMEOUT: Duration = Duration::from_secs(3);
/// 日志只从尾部回读这么多字节，避免把几十 MB 的推理日志整个读进内存。
const TAIL_WINDOW: u64 = 64 * 1024;
const TAIL_MAX_LINES: usize = 500;
const LOG_EXTENSIONS: [&str; 2] = ["log", "txt"];

// ---------------------------------------------------------------------------
// HTTP / TCP
// ---------------------------------------------------------------------------

/// 监控用的 HTTP 客户端：超时必须短，且**禁用代理**。
///
/// 环境里若设了 `HTTP_PROXY`，reqwest 默认会把 127.0.0.1 的请求也交给代理转发，
/// 结果就是「本机服务明明活着却报 down」。这些请求只针对 loopback，必须直连。
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .no_proxy()
            .build()
            .expect("构建监控 HTTP 客户端失败")
    })
}

/// 端口是否在监听。只建连接、不发任何字节，因此不会唤醒按需网关。
async fn tcp_up(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            TCP_TIMEOUT,
            tokio::net::TcpStream::connect(("127.0.0.1", port))
        )
        .await,
        Ok(Ok(_))
    )
}

async fn http_json(port: u16, path: &str) -> Option<Value> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let response = http_client().get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().await.ok()
}

// ---------------------------------------------------------------------------
// 探活
// ---------------------------------------------------------------------------

async fn probe_service(service: &LocalAiService) -> Value {
    let mut out = json!({
        "key": service.key,
        "port": service.port,
        "name": service.name,
        "desc": service.desc,
        "tcpOnly": service.tcp_only,
        "model": Value::Null,
        "tasks": [],
    });

    if service.tcp_only {
        // 端口在听就是 ready；不在听是 cold。按需网关空闲自停是设计内行为，
        // 把它标成故障会误导人，所以单独一个状态。
        out["state"] = json!(if tcp_up(service.port).await { "ready" } else { "cold" });
        return out;
    }

    if !tcp_up(service.port).await {
        out["state"] = json!("down");
        return out;
    }

    let Some(models) = http_json(service.port, "/v1/models").await else {
        out["state"] = json!("down");
        return out;
    };
    out["state"] = json!("ready");

    if let Some(name) = models
        .get("models")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
        .and_then(|first| first.get("name"))
        .and_then(Value::as_str)
    {
        // llama.cpp 给的是完整路径，界面只关心文件名
        let short = Path::new(name)
            .file_name()
            .map(|part| part.to_string_lossy().to_string())
            .unwrap_or_else(|| name.to_string());
        out["model"] = json!(short);
    }

    if let Some(slots) = http_json(service.port, "/slots")
        .await
        .and_then(|value| value.as_array().cloned())
    {
        let tasks: Vec<Value> = slots
            .iter()
            .filter(|slot| {
                slot.get("is_processing")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .map(|slot| {
                json!({
                    "task": slot.get("id_task").cloned().unwrap_or(Value::Null),
                    "promptTokens": slot.get("n_prompt_tokens").cloned().unwrap_or(Value::Null),
                    "processed": slot.get("n_prompt_tokens_processed").cloned().unwrap_or(Value::Null),
                    "cache": slot.get("n_prompt_tokens_cache").cloned().unwrap_or(Value::Null),
                    "predicted": slot
                        .get("n_predicted")
                        .or_else(|| slot.get("predicted_tokens"))
                        .cloned()
                        .unwrap_or(Value::Null),
                })
            })
            .collect();
        out["tasks"] = json!(tasks);
    }

    out
}

/// 并发跑所有服务的探活。串行在「某个端口被占住不响应」时会累加成 3 倍超时。
async fn probe_all(services: &[LocalAiService]) -> Vec<Value> {
    let mut set = tokio::task::JoinSet::new();
    for service in services {
        let service = service.clone();
        let key = service.key.clone();
        set.spawn(async move { (key, probe_service(&service).await) });
    }

    let mut by_key: HashMap<String, Value> = HashMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((key, value)) = joined {
            by_key.insert(key, value);
        }
    }

    // 按配置顺序还原；某个任务 panic 时补一条占位，不能让界面凭空少一行
    services
        .iter()
        .map(|service| {
            by_key.remove(&service.key).unwrap_or_else(|| {
                json!({
                    "key": service.key, "port": service.port, "name": service.name,
                    "desc": service.desc, "tcpOnly": service.tcp_only,
                    "state": "down", "model": Value::Null, "tasks": [],
                })
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 温度（smctemp 直读 SMC，免 sudo）
// ---------------------------------------------------------------------------

fn find_smctemp(configured: &str) -> Option<PathBuf> {
    paths::smctemp_candidates(configured)
        .into_iter()
        .find(|path| path.is_file())
}

fn run_smctemp(binary: &Path, arg: &str) -> Option<f64> {
    let mut child = Command::new(binary)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + SMCTEMP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }

    let mut raw = String::new();
    child.stdout.take()?.read_to_string(&mut raw).ok()?;

    // 输出形如 "56.1"，个别版本带单位（"56.1°C"），只取开头的数字部分
    let numeric: String = raw
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    numeric.parse::<f64>().ok()
}

fn read_temps(configured: &str) -> Value {
    let Some(binary) = find_smctemp(configured) else {
        return json!({ "cpu": Value::Null, "gpu": Value::Null, "smctemp": Value::Null });
    };
    json!({
        "cpu": run_smctemp(&binary, "-c"),
        "gpu": run_smctemp(&binary, "-g"),
        "smctemp": binary.to_string_lossy(),
    })
}

// ---------------------------------------------------------------------------
// 内存
// ---------------------------------------------------------------------------

/// `vm_statistics64`（`mach/vm_statistics.h`）的镜像。
///
/// **字段顺序与类型必须和内核头文件逐一对齐**：`host_statistics64` 是按缓冲区
/// 长度往这块内存里写数的，字段漏一个或类型写错就会读出垃圾。前 4 个计数是 32 位，
/// 其后转为 64 位；`#[repr(C)]` 的对齐规则与 C 编译器一致，顺序不错就不会错位。
/// 布局由 `vm_statistics64_layout_matches_kernel_header` 这个单测钉死。
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct VmStatistics64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

#[cfg(target_os = "macos")]
const HOST_VM_INFO64: i32 = 4;
/// 内核以「整数个数」为单位要缓冲区长度。由结构体自身大小推出，改了布局会自动跟着变。
#[cfg(target_os = "macos")]
const HOST_VM_INFO64_COUNT: u32 = (std::mem::size_of::<VmStatistics64>() / 4) as u32;
#[cfg(target_os = "macos")]
const KERN_SUCCESS: i32 = 0;

#[cfg(target_os = "macos")]
extern "C" {
    fn mach_host_self() -> u32;
    fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
}

#[cfg(target_os = "macos")]
/// 已安装内存的字节数（`hw.memsize`）。读不到返回 None。
fn physical_memory() -> Option<u64> {
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: 名字是以 NUL 结尾的静态字节串；`value` / `size` 都是栈上变量，
    // 缓冲区大小如实告知内核，写入不会越界。
    let status = unsafe {
        libc::sysctlbyname(
            b"hw.memsize\0".as_ptr() as *const libc::c_char,
            &mut value as *mut u64 as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (status == 0 && size == std::mem::size_of::<u64>()).then_some(value)
}

#[cfg(target_os = "macos")]
/// 系统页大小。Apple Silicon 上是 16 KiB、Intel 机器上是 4 KiB，不能写死。
fn page_size() -> u64 {
    // SAFETY: sysconf 只读系统常量，`_SC_PAGESIZE` 是合法参数。
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as u64
    } else {
        // 兜底值取 Apple Silicon 的常见页大小。几乎不会走到，且猜错只影响数值精度。
        16 * 1024
    }
}

/// 内存占用，口径对齐「活动监视器」的 Memory Used：
/// 已用 = App 内存（内部页 − 可清除页）+ 联动内存（wired）+ 已压缩内存。
///
/// 直接调 mach 口读，不起 `vm_stat` 之类的子进程 —— 面板每 5 秒刷一次，
/// 每轮 fork 一个进程不值当。
#[cfg(target_os = "macos")]
pub(crate) fn read_memory() -> Value {
    let mut stats = VmStatistics64::default();
    let mut count = HOST_VM_INFO64_COUNT;
    // SAFETY: `mach_host_self` 无参数、无副作用；`host_statistics64` 最多往 `stats`
    // 写 `count` 个整数，而 `count` 正是由 `stats` 自身大小推出来的。
    let status = unsafe {
        host_statistics64(
            mach_host_self(),
            HOST_VM_INFO64,
            &mut stats as *mut VmStatistics64 as *mut i32,
            &mut count,
        )
    };
    let total = match physical_memory() {
        Some(total) if status == KERN_SUCCESS && total > 0 => total,
        _ => return no_memory(),
    };

    let page = page_size();
    let app_pages =
        u64::from(stats.internal_page_count).saturating_sub(u64::from(stats.purgeable_count));
    let used = (app_pages + u64::from(stats.wire_count) + u64::from(stats.compressor_page_count))
        .saturating_mul(page)
        .min(total);

    json!({
        "total": total,
        "used": used,
        // 一位小数：面板上「38.7%」比整数更能反映缓存占用的小幅波动
        "usedPercent": ((used as f64 / total as f64) * 1000.0).round() / 10.0,
    })
}

/// Windows 内存占用：`GlobalMemoryStatusEx` 直接给出总内存与可用内存。
///
/// 口径与 mac 版一致：已用 = 总量 − 可用量；`usedPercent` 保留一位小数。
#[cfg(target_os = "windows")]
pub(crate) fn read_memory() -> Value {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    // 必须先把 dwLength 填成结构体大小，否则 API 直接返回失败。
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return no_memory();
    }
    let total = status.ullTotalPhys;
    let used = total.saturating_sub(status.ullAvailPhys);
    json!({
        "total": total,
        "used": used,
        "usedPercent": ((used as f64 / total.max(1) as f64) * 1000.0).round() / 10.0,
    })
}

// ---------------------------------------------------------------------------
// 日志
// ---------------------------------------------------------------------------

pub fn log_dir(config: &LocalAiConfig) -> PathBuf {
    if config.log_dir.trim().is_empty() {
        paths::default_log_dir()
    } else {
        paths::expand_tilde(&config.log_dir)
    }
}

/// 去掉 ANSI 转义序列（颜色码）。llama.cpp 往终端写日志时会带上它们。
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(current) = chars.next() {
        if current != '\u{1b}' {
            out.push(current);
            continue;
        }
        // CSI 序列：ESC [ 参数… 终止字节(@~)
        if chars.peek() == Some(&'[') {
            chars.next();
            while let Some(&next) = chars.peek() {
                chars.next();
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        }
    }
    out
}

/// 把前端传来的文件名解析成日志目录内的真实路径。
///
/// 只接受目录内的单层普通文件：先挡掉路径分隔符与隐藏文件，再用 canonicalize
/// 比对父目录——这样符号链接指向目录外也会被拦住。
fn resolve_log_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.starts_with('.')
    {
        return None;
    }

    let extension = Path::new(trimmed).extension()?.to_str()?.to_ascii_lowercase();
    if !LOG_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }

    let real = dir.join(trimmed).canonicalize().ok()?;
    let base = dir.canonicalize().ok()?;
    (real.parent() == Some(base.as_path())).then_some(real)
}

/// 目录内的日志文件清单，最近写过的排前面。
pub fn list_log_files(dir: &Path) -> Vec<Value> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut files: Vec<(i64, Value)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            if name.starts_with('.') {
                return None;
            }
            let extension = path.extension()?.to_str()?.to_ascii_lowercase();
            if !LOG_EXTENSIONS.contains(&extension.as_str()) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|delta| delta.as_millis() as i64)
                .unwrap_or(0);
            Some((
                mtime,
                json!({ "name": name, "size": meta.len(), "mtime": mtime }),
            ))
        })
        .collect();

    files.sort_by(|left, right| right.0.cmp(&left.0));
    files.into_iter().map(|(_, value)| value).collect()
}

/// 读日志尾部若干行。文件不存在或路径不合法都返回 `error`，不 panic。
pub fn tail_log(dir: &Path, name: &str, lines: usize) -> Value {
    let Some(path) = resolve_log_path(dir, name) else {
        return json!({ "error": "日志文件不可访问" });
    };
    let Ok(mut file) = fs::File::open(&path) else {
        return json!({ "error": "无法打开日志文件" });
    };

    let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if size > TAIL_WINDOW {
        let _ = file.seek(SeekFrom::Start(size - TAIL_WINDOW));
    }

    let mut raw = Vec::new();
    let _ = file.take(TAIL_WINDOW).read_to_end(&mut raw);
    // 从窗口中间截断时首个字符可能落在多字节边界上，lossy 解码不会失败
    let text = String::from_utf8_lossy(&raw);

    // 日志末尾几乎总是带换行，直接按 \n 切会多切出一个空行。末尾的空白行对
    // 阅读没有价值，却会让界面底部一直挂着一条空白，所以先裁掉。
    let text = text.trim_end_matches(['\r', '\n']);

    let wanted = lines.clamp(1, TAIL_MAX_LINES);
    let kept: Vec<String> = text
        .split('\n')
        .rev()
        .take(wanted)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|line| strip_ansi(line.trim_end_matches('\r')))
        .collect();

    json!({
        "file": path.file_name().map(|part| part.to_string_lossy().to_string()).unwrap_or_default(),
        "size": size,
        "lines": kept,
    })
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 温度读取整体失败（阻塞任务 panic）时的占位，字段含义与正常返回值一致。
fn no_temps() -> Value {
    json!({ "cpu": Value::Null, "gpu": Value::Null, "smctemp": Value::Null })
}

/// 内存读取失败时的占位，字段含义与正常返回值一致。
fn no_memory() -> Value {
    json!({ "total": Value::Null, "used": Value::Null, "usedPercent": Value::Null })
}

/// 探活一轮。功能关闭时直接返回，不建连接、不读文件、不起进程。
pub async fn status() -> Value {
    let config = paths::load_config().local_ai;
    if !config.enabled {
        return json!({
            "enabled": false,
            "time": paths::now_ms(),
            "services": [],
            "tasks": [],
            "memory": Value::Null,
            "notes": [],
            "logs": { "dir": log_dir(&config).to_string_lossy(), "dirExists": false, "files": [] },
        });
    }

    let dir = log_dir(&config);
    let smctemp_configured = config.smctemp_path.clone();
    let services = config.services.clone();

    // 温度要起外部进程读 SMC，内存要调 mach 口，都是阻塞操作；日志要碰磁盘。
    // 三者都丢到阻塞线程池，别占住异步执行器。
    let sys_task = tauri::async_runtime::spawn_blocking(move || {
        (read_temps(&smctemp_configured), read_memory())
    });
    let dir_for_logs = dir.clone();
    let logs_task = tauri::async_runtime::spawn_blocking(move || {
        let exists = dir_for_logs.is_dir();
        let files = if exists {
            list_log_files(&dir_for_logs)
        } else {
            Vec::new()
        };
        (exists, files)
    });

    let results = probe_all(&services).await;
    let (temps, memory) = sys_task
        .await
        .unwrap_or_else(|_| (no_temps(), no_memory()));
    let (dir_exists, log_files) = logs_task
        .await
        .unwrap_or_else(|_| (false, Vec::new()));

    let mut tasks = Vec::new();
    for service in &results {
        if let Some(list) = service["tasks"].as_array() {
            for task in list {
                let mut task = task.clone();
                task["service"] = service["name"].clone();
                task["port"] = service["port"].clone();
                tasks.push(task);
            }
        }
    }

    // 把「为什么看起来不对」直接讲清楚，而不是只给一排灰点
    let mut notes = Vec::new();
    if !dir_exists {
        notes.push(format!("日志目录不存在：{}", dir.display()));
    } else if log_files.is_empty() {
        notes.push("日志目录里没有 .log / .txt 文件".to_string());
    }
    if temps["smctemp"].is_null() {
        notes.push("未找到 smctemp，温度不可读".to_string());
    }
    if !results.iter().any(|item| item["state"] == json!("ready")) {
        notes.push("没有检测到正在运行的后端。这台电脑若没有部署本地 AI，可以把本功能关掉。".to_string());
    }

    json!({
        "enabled": true,
        "time": paths::now_ms(),
        "temps": temps,
        "memory": memory,
        "services": results,
        "tasks": tasks,
        "logs": {
            "dir": dir.to_string_lossy(),
            "dirExists": dir_exists,
            "files": log_files,
        },
        "notes": notes,
    })
}

/// 自动检测本机是否部署了本地 AI。只读，不改配置——是否启用由用户决定。
pub async fn detect() -> Value {
    let config = paths::load_config().local_ai;
    let dir = log_dir(&config);
    let smctemp_configured = config.smctemp_path.clone();
    let services = config.services.clone();

    let mut set = tokio::task::JoinSet::new();
    for service in &services {
        let port = service.port;
        set.spawn(async move { (port, tcp_up(port).await) });
    }
    let mut listening: HashMap<u16, bool> = HashMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((port, up)) = joined {
            listening.insert(port, up);
        }
    }

    let dir_for_logs = dir.clone();
    let (smctemp, log_files, dir_exists) = tauri::async_runtime::spawn_blocking(move || {
        let found = find_smctemp(&smctemp_configured).map(|path| path.to_string_lossy().to_string());
        let exists = dir_for_logs.is_dir();
        let count = if exists {
            list_log_files(&dir_for_logs).len()
        } else {
            0
        };
        (found, count, exists)
    })
    .await
    .unwrap_or((None, 0, false));

    let ports: Vec<Value> = services
        .iter()
        .map(|service| {
            json!({
                "key": service.key,
                "port": service.port,
                "name": service.name,
                "tcpOnly": service.tcp_only,
                "listening": listening.get(&service.port).copied().unwrap_or(false),
            })
        })
        .collect();

    let any_listening = ports
        .iter()
        .any(|item| item["listening"] == json!(true));

    json!({
        // 有服务在听，或存在约定日志目录，都算「本机部署过本地 AI」
        "deployed": any_listening || dir_exists,
        "ports": ports,
        "logDir": dir.to_string_lossy(),
        "logDirExists": dir_exists,
        "logFiles": log_files,
        "smctemp": smctemp,
    })
}

/// 只改本地 AI 这几项，其余配置原样保留。
pub fn update_config(
    enabled: bool,
    log_dir: String,
    smctemp_path: String,
) -> Result<paths::AppConfig, String> {
    let mut config = paths::load_config();
    config.local_ai.enabled = enabled;
    config.local_ai.log_dir = log_dir;
    config.local_ai.smctemp_path = smctemp_path;
    let config = config.normalized();
    paths::save_config(&config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-localai-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).expect("建临时目录应成功");
        dir
    }

    #[test]
    fn ansi_sequences_are_stripped() {
        assert_eq!(strip_ansi("\u{1b}[1;32mok\u{1b}[0m"), "ok");
        assert_eq!(strip_ansi("\u{1b}[Kplain"), "plain");
        assert_eq!(strip_ansi("无转义原样"), "无转义原样");
        // 结尾是被截断的转义序列，不能把它吐回界面
        assert_eq!(strip_ansi("abc\u{1b}[0"), "abc");
    }

    #[test]
    fn resolve_rejects_path_traversal_and_foreign_files() {
        let dir = temp_dir();
        fs::write(dir.join("ok.log"), "x").expect("写文件应成功");
        fs::write(dir.join("note.txt"), "x").expect("写文件应成功");
        fs::write(dir.join("secret.key"), "x").expect("写文件应成功");

        assert!(resolve_log_path(&dir, "ok.log").is_some());
        assert!(resolve_log_path(&dir, "note.txt").is_some());

        assert!(resolve_log_path(&dir, "../ok.log").is_none(), "上跳应被拒");
        assert!(resolve_log_path(&dir, "sub/ok.log").is_none(), "子目录应被拒");
        assert!(resolve_log_path(&dir, "secret.key").is_none(), "扩展名不在白名单");
        assert!(resolve_log_path(&dir, ".DS_Store").is_none(), "隐藏文件应被拒");
        assert!(resolve_log_path(&dir, "").is_none());
        assert!(resolve_log_path(&dir, "missing.log").is_none(), "不存在的文件应被拒");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_escaping_the_log_dir_is_rejected() {
        let dir = temp_dir();
        let outside = temp_dir();
        fs::write(outside.join("target.log"), "x").expect("写文件应成功");
        std::os::unix::fs::symlink(outside.join("target.log"), dir.join("link.log"))
            .expect("建符号链接应成功");

        assert!(
            resolve_log_path(&dir, "link.log").is_none(),
            "指向目录外的符号链接必须被拒"
        );

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn log_listing_keeps_only_log_files_and_sorts_by_mtime() {
        let dir = temp_dir();
        fs::write(dir.join("a.log"), "a").expect("写文件应成功");
        fs::write(dir.join("b.log"), "b").expect("写文件应成功");
        fs::write(dir.join("c.txt"), "c").expect("写文件应成功");
        fs::write(dir.join("d.md"), "d").expect("写文件应成功");
        fs::create_dir(dir.join("e.log")).expect("建同名目录应成功");

        let files = list_log_files(&dir);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|item| item["name"].as_str())
            .collect();

        assert_eq!(names.len(), 3, "只应列出 .log / .txt，且同名目录不算文件");
        assert!(names.contains(&"a.log") && names.contains(&"b.log") && names.contains(&"c.txt"));
        assert!(!names.contains(&"d.md"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_log_dir_yields_empty_listing_not_panic() {
        let dir = std::env::temp_dir().join("wb-localai-does-not-exist-xyz");
        assert!(list_log_files(&dir).is_empty());
    }

    #[test]
    fn tail_returns_last_lines_only() {
        let dir = temp_dir();
        let mut file = fs::File::create(dir.join("run.log")).expect("建文件应成功");
        for index in 1..=50 {
            writeln!(file, "line {index}").expect("写日志应成功");
        }
        file.flush().expect("flush 应成功");
        drop(file);

        let result = tail_log(&dir, "run.log", 3);
        let lines: Vec<&str> = result["lines"]
            .as_array()
            .expect("应返回数组")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(lines, vec!["line 48", "line 49", "line 50"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_without_trailing_newline_keeps_last_line() {
        let dir = temp_dir();
        fs::write(dir.join("nonl.log"), "alpha\nbeta").expect("写文件应成功");

        let result = tail_log(&dir, "nonl.log", 5);
        let lines: Vec<&str> = result["lines"]
            .as_array()
            .expect("应返回数组")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(lines, vec!["alpha", "beta"], "末尾没有换行时不能丢掉最后一行");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_on_rejected_name_reports_error_instead_of_reading() {
        let dir = temp_dir();
        fs::write(dir.join("ok.log"), "x").expect("写文件应成功");
        let result = tail_log(&dir, "../../etc/passwd", 10);
        assert!(result.get("error").is_some(), "越权路径应返回错误");
        assert!(result.get("lines").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_log_dir_config_falls_back_to_convention() {
        let config = LocalAiConfig::default();
        assert_eq!(log_dir(&config), paths::default_log_dir());

        let custom = LocalAiConfig {
            log_dir: "~/custom/logs".to_string(),
            ..LocalAiConfig::default()
        };
        assert_eq!(
            log_dir(&custom),
            paths::home_dir().join("custom").join("logs"),
            "自定义路径应展开 ~"
        );
    }

    fn service(key: &str, port: u16, tcp_only: bool) -> LocalAiService {
        LocalAiService {
            key: key.to_string(),
            port,
            name: key.to_string(),
            desc: String::new(),
            tcp_only,
        }
    }

    /// 借一个端口号，随即释放，保证探测时该端口确实没人监听。
    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("应能绑定端口");
        let port = listener.local_addr().expect("应能取到地址").port();
        drop(listener);
        port
    }

    #[tokio::test]
    async fn listening_port_is_ready_and_tcp_only_service_never_calls_http() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("应能绑定端口");
        let port = listener.local_addr().expect("应能取到地址").port();

        let results = probe_all(&[service("gateway", port, true)]).await;
        assert_eq!(results[0]["state"], json!("ready"), "端口在听即 ready");
        // 这条是硬约束：对TCP-only 服务发 HTTP 会把按需网关白白唤醒
        assert!(results[0]["model"].is_null(), "TCP-only 服务不得查询模型");
        assert!(
            results[0]["tasks"].as_array().expect("tasks 应为数组").is_empty(),
            "TCP-only 服务不得查询 slots"
        );

        drop(listener);
    }

    #[tokio::test]
    async fn nothing_listening_gives_cold_for_gateway_and_down_for_backend() {
        let port = free_port();
        let results = probe_all(&[
            service("gateway", port, true),
            service("text", port, false),
        ])
        .await;

        assert_eq!(
            results[0]["state"],
            json!("cold"),
            "网关端口不通是空闲（cold），不能报成故障"
        );
        assert_eq!(
            results[1]["state"],
            json!("down"),
            "推理后端端口不通才是 down"
        );
        // 顺序必须与配置一致，否则界面上的三张卡片会串位
        assert_eq!(results[0]["key"], json!("gateway"));
        assert_eq!(results[1]["key"], json!("text"));
    }

    #[test]
    fn default_config_monitors_gateway_text_and_vision() {
        let services = LocalAiConfig::default().services;
        assert_eq!(services.len(), 3);
        assert_eq!(services[0].port, 8080);
        assert!(services[0].tcp_only, "默认配置里网关必须是 TCP-only");
        assert!(!services[1].tcp_only && !services[2].tcp_only);
        assert_eq!((services[1].port, services[2].port), (8081, 18080));
    }

    #[test]
    fn vm_statistics64_layout_matches_kernel_header() {
        // Darwin 上 sizeof(vm_statistics64) = 152 字节 = 38 个整数。
        // 大小一旦对不上，就说明字段类型或顺序跟头文件脱节了 —— 此时
        // host_statistics64 会按内核认的长度往缓冲区里写，读出来的数是错的。
        assert_eq!(std::mem::size_of::<VmStatistics64>(), 152);
        assert_eq!(HOST_VM_INFO64_COUNT, 38);

        // 抽查关键偏移：4 个 32 位计数后紧跟 64 位字段，编译器按 8 字节对齐排布
        assert_eq!(std::mem::offset_of!(VmStatistics64, wire_count), 12);
        assert_eq!(std::mem::offset_of!(VmStatistics64, zero_fill_count), 16);
        assert_eq!(
            std::mem::offset_of!(VmStatistics64, compressor_page_count),
            128
        );
        assert_eq!(std::mem::offset_of!(VmStatistics64, internal_page_count), 140);
    }

    #[test]
    fn page_size_is_a_plausible_power_of_two() {
        let size = page_size();
        assert!(size.is_power_of_two(), "页大小应为 2 的幂，实际 {size}");
        assert!(
            (4096..=65536).contains(&size),
            "页大小应在 4 KiB ~ 64 KiB 之间，实际 {size}"
        );
    }

    /// 真读一次本机内存。这条测试的价值在于：它验证的是 mach 调用链本身，
    /// 而不只是我们自己构造的数字 —— 结构体布局写错时它会立刻失败。
    #[test]
    fn memory_reading_is_within_physical_bounds() {
        let memory = read_memory();
        let total = memory["total"].as_u64().expect("本机应能读到 hw.memsize");
        let used = memory["used"].as_u64().expect("应能算出已用内存");
        let percent = memory["usedPercent"].as_f64().expect("应能算出占用率");

        assert!(total > 0);
        assert!(used > 0, "任何活着的系统，已用内存都大于 0");
        assert!(used <= total, "已用内存不可能超过装机总量");
        assert!((0.0..=100.0).contains(&percent), "占用率应在 0~100 之间");
        assert_eq!(
            total,
            physical_memory().expect("hw.memsize 应可读"),
            "总量应与 sysctl 独立读到的值一致，说明页大小没乘错"
        );
    }
}
