//! 免费渠道（本地网关）配置：把 `127.0.0.1:8901/admin/api/*` 代理给前端。
//!
//! 网关是一个独立的 LaunchAgent（`ai.wbgateway.openrouter-free`），有它时本模块
//! 只是转发一层；没装或没起时 `state()` 返回 `offline` 而不是报错，面板据此显示
//! 「网关未运行」，不会让整个界面变成一个红条。
//!
//! 仍然遵守本应用的总原则：**不新开端口、前端不直接发 HTTP**，出站只有这一条
//! 到本机回环地址的连接，且不带任何来自外部的输入拼进 URL 路径（一律走白名单路径）。

use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// OrcaTerm 模型显示名：只给「单独注入」的模型用，键是网关里的真实 ID。
const ORCATERM_LABELS: &[(&str, &str)] = &[
    ("TokenHub/deepseek-v4.1-flash", "DeepSeek V4.1 Flash"),
    ("TokenHub/deepseek-v4-flash",   "DeepSeek V4 Flash"),
    ("TokenHub/deepseek-v4-pro",     "DeepSeek V4 Pro"),
    ("TokenHub/kimi-k3",             "Kimi K3"),
    ("TokenHub/glm-5.3-flash",       "GLM 5.3 Flash"),
    ("TokenHub/glm-5.3",             "GLM 5.3"),
    ("TokenHub/glm-5.2",             "GLM 5.2"),
    ("Hunyuan3/hy3",                 "Hunyuan 3"),
];

/// 官方清单还没拉到时用的兜底名称；正常情况名字来自 config.json 的 doubao.catalog（动态，含第三方模型）
const DOUBAO_LABELS: &[(&str, &str)] = &[
    ("doubao",       "快速"),
    ("doubao-turbo", "专家"),
];

/// 三个「网页额度 / OAuth」平台。注入哪些模型由网关 config.json 的 `inject` 决定：
///   enabled —— 平台开关，关掉就把该平台条目从 models.json 全部清掉
///   inject  —— 真正写进 WorkBuddy 的条目，默认只有 `auto`（单独模型默认不写，模型列表才不臃肿）
const PROVIDERS: &[(&str, &str)] = &[
    ("orcaterm", "OrcaTerm"),
    ("doubao", "Doubao"),
    ("antigravity", "Antigravity"),
];

const PORT: u16 = 8901;
const LAUNCH_LABEL: &str = "ai.wbgateway.openrouter-free";
/// app 内置网关资源目录（在 bundle 的 Resources 下），首次启动解包到用户目录。
const BUNDLE_DIR: &str = "gateway-bundle";
/// 随包更新的脚本；config.json 不在其中 —— 那份一旦落盘就是用户私有配置，绝不覆盖。
const BUNDLE_FILES: &[&str] = &[
    "gateway.mjs", "admin.html", "orcaterm_auth.mjs", "doubao_bridge.mjs", "antigravity.mjs",
    "run.sh", "node", "README.md",
];

fn gateway_dir() -> PathBuf {
    crate::paths::home_dir().join(".wb-checkin-gateway")
}

fn plist_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        crate::paths::home_dir()
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCH_LABEL}.plist"))
    }
    // Windows 没有 LaunchAgent，诊断面板里这一项改指 run.cmd（用户能双击自查）
    #[cfg(target_os = "windows")]
    {
        run_cmd_path()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        gateway_dir().join("run.sh")
    }
}

fn copy_file(src: &Path, dst: &Path, executable: bool) -> Result<(), String> {
    std::fs::copy(src, dst)
        .map_err(|error| format!("复制 {} 失败：{}", src.display(), error))?;
    #[cfg(unix)]
    if executable {
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("设置可执行权限失败：{}", error))?;
    }
    Ok(())
}

/// 从 app Resources 复制到家目录的 node 会被 macOS 打上 com.apple.provenance 隔离标记，
/// 直接执行会被 SIGKILL（exit 137）—— 症状就是扫码登录点了没反应、登录器秒退。
/// 必须 adhoc 重签才能清掉标记。
#[cfg(target_os = "macos")]
fn resign_node(path: &Path) -> Result<(), String> {
    let output = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-", &path.to_string_lossy()])
        .output()
        .map_err(|error| format!("调用 codesign 失败：{error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
}

/// node 能不能真跑起来。只看 -x 权限不够：带 provenance 的文件有执行位但一跑就被杀。
#[cfg(target_os = "macos")]
fn node_runs(path: &Path) -> bool {
    Command::new(path)
        .args(["-e", ""])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 每次启动自检：内置 node 被系统杀（137）就重签一次，让已装旧版的用户自动修复。
#[cfg(target_os = "macos")]
fn ensure_node_runnable() {
    let node = gateway_dir().join("node");
    if !node.exists() || node_runs(&node) {
        return;
    }
    eprintln!("[freehub] 内置 node 无法执行，尝试重签…");
    match resign_node(&node) {
        Ok(()) if node_runs(&node) => eprintln!("[freehub] node 重签成功，已恢复"),
        Ok(()) => eprintln!("[freehub] node 重签后仍无法执行"),
        Err(error) => eprintln!("[freehub] node 重签失败：{error}"),
    }
}

// ---------------------------------------------------------------------------
// Windows：没有 launchd，网关是一个独立 node 进程 —— app 启动时拉起、脚本变了就杀掉重拉。
// 端口探活比查进程名可靠：node.exe 可能是别家的进程。
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(target_os = "windows")]
fn gateway_pid_path() -> PathBuf {
    gateway_dir().join(".gateway.pid")
}

#[cfg(target_os = "windows")]
fn gateway_alive() -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], PORT).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

#[cfg(target_os = "windows")]
fn run_cmd_path() -> PathBuf {
    gateway_dir().join("run.cmd")
}

/// 杀掉网关进程：pid 由 gateway.mjs 启动时写进 `.gateway.pid`。
/// 只认这个 pid，别用 `taskkill /IM node.exe` —— 会把用户自己跑的 node 一起杀掉。
#[cfg(target_os = "windows")]
fn kill_gateway() {
    use std::os::windows::process::CommandExt;
    let pid = std::fs::read_to_string(gateway_pid_path())
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok());
    if let Some(pid) = pid {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }
    let _ = std::fs::remove_file(gateway_pid_path());
    // 等端口真正释放，否则新进程 bind 会 EADDRINUSE
    for _ in 0..20 {
        if !gateway_alive() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "windows")]
fn spawn_gateway() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    let dir = gateway_dir();
    let builtin = dir.join("node.exe");
    let node = if builtin.exists() { builtin } else { PathBuf::from("node") };
    let log = |name: &str| -> Result<std::fs::File, String> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(name))
            .map_err(|error| format!("打开 {name} 失败：{error}"))
    };
    Command::new(&node)
        .arg(dir.join("gateway.mjs"))
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log("stdout.log")?))
        .stderr(Stdio::from(log("stderr.log")?))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|error| format!("启动网关进程失败：{error}（node={}）", node.display()))?;
    Ok(())
}

/// 给用户留一个能自己双击试的入口（诊断面板里会显示这个路径）。
#[cfg(target_os = "windows")]
fn write_run_cmd(dir: &Path) -> Result<(), String> {
    let body = "@echo off\r\nchcp 65001 >nul\r\ncd /d \"%~dp0\"\r\n\
if exist node.exe ( node.exe gateway.mjs ) else ( node gateway.mjs )\r\n";
    std::fs::write(dir.join("run.cmd"), body).map_err(|error| format!("写 run.cmd 失败：{error}"))
}

#[cfg(target_os = "windows")]
fn ensure_installed_windows(resource_dir: &Path) -> Result<bool, String> {
    let bundle = resource_dir.join(BUNDLE_DIR);
    if !bundle.is_dir() {
        return Err(format!("app 内置网关缺失：{}", bundle.display()));
    }
    let dir = gateway_dir();
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建网关目录失败：{error}"))?;
    let cfg = dir.join("config.json");
    let first_install = !cfg.exists();
    let version = env!("CARGO_PKG_VERSION");
    let marker = dir.join(".bundle-version");
    let installed_version = std::fs::read_to_string(&marker).unwrap_or_default();
    let version_changed = installed_version.trim() != version;
    let mut changed = false;

    // 脚本逐个按内容比对：版本号没动但打了新包也能同步过去
    for name in BUNDLE_FILES
        .iter()
        .filter(|name| **name != "node" && **name != "run.sh")
    {
        let src = bundle.join(name);
        let dst = dir.join(name);
        if same_content(&src, &dst) {
            continue;
        }
        copy_file(&src, &dst, false)?;
        eprintln!("[freehub] gateway script updated: {name}");
        changed = true;
    }

    // node.exe 同样是几十 MB 的二进制，只在首次安装或版本变化时复制
    let node = dir.join("node.exe");
    let src_node = bundle.join("node.exe");
    if first_install || !node.exists() || version_changed {
        if src_node.exists() {
            copy_file(&src_node, &node, false)?;
            changed = true;
        } else {
            eprintln!("[freehub] bundle has no node.exe, falling back to system node");
        }
    }

    write_run_cmd(&dir)?;
    if first_install {
        copy_file(&bundle.join("config.json"), &cfg, false)?;
    }
    if version_changed {
        std::fs::write(&marker, version)
            .map_err(|error| format!("写版本标记失败：{error}"))?;
    }
    Ok(changed)
}

/// 两个文件字节完全相同？只比对大小 + 全量内容，脚本都是几十 KB，开销可忽略。
fn same_content(src: &Path, dst: &Path) -> bool {
    match (std::fs::metadata(src), std::fs::metadata(dst)) {
        (Ok(a), Ok(b)) if a.len() == b.len() => {}
        _ => return false,
    }
    match (std::fs::read(src), std::fs::read(dst)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 把 app 内置的网关解包到 ~/.wb-checkin-gateway（幂等）：
/// - 首次：复制全部脚本 + 无凭据的 config 模板（0600）；
/// - 升级：只覆盖脚本，config 原样保留（里面有用户自己申请的 key / cookie）；
/// - plist 每次按当前 $HOME 重写 —— fix 掉旧版写死 /Users/apreslavie 的问题。
///
/// 返回 `true` 表示脚本/二进制被改写过，调用方必须重启网关进程 ——
/// launchd 只在进程退出时重新读脚本，否则网关一直跑内存里的旧路由，
/// 表现就是升级后点豆包登录回「未知接口」(404)。
pub fn ensure_installed(resource_dir: &Path) -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        let bundle = resource_dir.join(BUNDLE_DIR);
        if !bundle.is_dir() {
            return Err(format!("app 内置网关缺失：{}", bundle.display()));
        }
        let dir = gateway_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("创建网关目录失败：{}", error))?;
        let cfg = dir.join("config.json");
        let first_install = !cfg.exists();
        let version = env!("CARGO_PKG_VERSION");
        let marker = dir.join(".bundle-version");
        let installed_version = std::fs::read_to_string(&marker).unwrap_or_default();
        let version_changed = installed_version.trim() != version;
        let mut changed = false;

        // 脚本逐个按内容比对：版本号相同但打了新包（或上次覆盖失败）也能同步过去。
        for name in BUNDLE_FILES.iter().filter(|name| **name != "node") {
            let src = bundle.join(name);
            let dst = dir.join(name);
            if same_content(&src, &dst) {
                continue;
            }
            copy_file(&src, &dst, *name == "run.sh")?;
            eprintln!("[freehub] 网关脚本已更新：{name}");
            changed = true;
        }

        // node 是 112MB 的二进制，不参与逐字节比对：首次安装或版本号变化时才复制。
        let node = dir.join("node");
        if first_install || !node.exists() || (version_changed && !installed_version.trim().is_empty()) {
            copy_file(&bundle.join("node"), &node, true)?;
            // 复制后重签 node：跨目录复制会带 provenance，不重签会被系统 SIGKILL
            if let Err(error) = resign_node(&node) {
                eprintln!("[freehub] node 重签失败：{error}");
            }
            changed = true;
        }

        if first_install {
            copy_file(&bundle.join("config.json"), &cfg, false)?;
            #[cfg(unix)]
            std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("设置配置权限失败：{}", error))?;
        }
        if version_changed {
            std::fs::write(&marker, version)
                .map_err(|error| format!("写版本标记失败：{}", error))?;
        }

        // 动态生成 plist：路径全用当前用户的 $HOME，不再写死任何用户名。
        let plist = plist_path();
        if let Some(parent) = plist.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("创建 LaunchAgents 目录失败：{}", error))?;
        }
        let run = dir.join("run.sh").display().to_string();
        let work = dir.display().to_string();
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\"><dict>\n\
<key>Label</key><string>{label}</string>\n\
<key>ProgramArguments</key><array><string>/bin/bash</string><string>{run}</string></array>\n\
<key>WorkingDirectory</key><string>{work}</string>\n\
<key>RunAtLoad</key><true/>\n\
<key>KeepAlive</key><true/>\n\
<key>ThrottleInterval</key><integer>10</integer>\n\
<key>StandardOutPath</key><string>{work}/stdout.log</string>\n\
<key>StandardErrorPath</key><string>{work}/stderr.log</string>\n\
</dict></plist>\n",
            label = LAUNCH_LABEL,
            run = run,
            work = work,
        );
        std::fs::write(&plist, xml)
            .map_err(|error| format!("写 LaunchAgent plist 失败：{}", error))?;
        Ok(changed)
    }
    #[cfg(target_os = "windows")]
    {
        ensure_installed_windows(resource_dir)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = resource_dir;
        Ok(false)
    }
}

/// 强制重启常驻网关（`kickstart -k`）。脚本被覆盖后必须走这一步，
/// 否则进程里跑的还是旧代码 —— 升级后新接口全变 404「未知接口」。
#[cfg(target_os = "macos")]
fn launchctl_kickstart() -> Result<(), String> {
    let script = format!("/bin/launchctl kickstart -k \"gui/$(id -u)/{LAUNCH_LABEL}\"");
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .output()
        .map_err(|error| format!("调用 launchctl 失败：{error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if detail.is_empty() { "kickstart 失败".to_string() } else { detail })
}

/// 服务已加载就跳过 bootstrap；未加载则按当前 plist 注册（不覆盖用户已有服务）。
#[cfg(target_os = "macos")]
fn launchctl_bootstrap() -> Result<(), String> {
    let uid_output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map_err(|error| format!("读取 uid 失败：{}", error))?;
    let uid = String::from_utf8_lossy(&uid_output.stdout).trim().to_string();
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{LAUNCH_LABEL}");
    let loaded = Command::new("/bin/launchctl")
        .args(["print", &service])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if loaded {
        return Ok(());
    }
    let output = Command::new("/bin/launchctl")
        .args(["bootstrap", &domain, &plist_path().to_string_lossy()])
        .output()
        .map_err(|error| format!("调用 launchctl 失败：{}", error))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    // 并发启动或旧接口加载过时会报 already loaded，视为成功
    if detail.contains("already") || detail.contains("service already") {
        return Ok(());
    }
    Err(if detail.is_empty() { "bootstrap 失败".to_string() } else { detail })
}

/// app 启动钩子调用：首次解包 + 注册常驻服务。失败只写日志，不阻断签到主流程。
pub fn bootstrap_on_launch(resource_dir: &Path) {
    #[cfg(target_os = "macos")]
    {
        match ensure_installed(resource_dir) {
            Ok(changed) => {
                match launchctl_bootstrap() {
                    Ok(()) => eprintln!("[freehub] builtin gateway installed / loaded"),
                    Err(error) => eprintln!("[freehub] builtin gateway bootstrap failed: {error}"),
                }
                // 脚本有变化必须重启进程：launchd 不会因为文件改动去重载，
                // 不重启就一直吃旧路由（升级后豆包 / Antigravity 接口全部 404）。
                if changed {
                    match launchctl_kickstart() {
                        Ok(()) => eprintln!("[freehub] gateway restarted to load new scripts"),
                        Err(error) => eprintln!("[freehub] gateway restart failed: {error}"),
                    }
                }
            }
            Err(error) => eprintln!("[freehub] builtin gateway bootstrap failed: {error}"),
        }
        // 旧版本解包时没重签的 node：每次启动自检一次，发现被杀就重签自愈
        ensure_node_runnable();
    }
    #[cfg(target_os = "windows")]
    {
        match ensure_installed_windows(resource_dir) {
            Ok(changed) => {
                if !gateway_alive() {
                    match spawn_gateway() {
                        Ok(()) => eprintln!("[freehub] gateway started"),
                        Err(error) => eprintln!("[freehub] gateway start failed: {error}"),
                    }
                } else if changed {
                    // 脚本变了但进程还在跑旧代码：杀掉重拉，否则新接口全 404
                    kill_gateway();
                    match spawn_gateway() {
                        Ok(()) => eprintln!("[freehub] gateway restarted to load new scripts"),
                        Err(error) => eprintln!("[freehub] gateway restart failed: {error}"),
                    }
                }
            }
            Err(error) => eprintln!("[freehub] builtin gateway install failed: {error}"),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = resource_dir;
    }
    // 装了 app 就该能在 WorkBuddy 里选到模型：自动把条目写进 ~/.workbuddy/models.json
    match inject_workbuddy_models() {
        Ok(message) => eprintln!("[freehub] workbuddy models: {message}"),
        Err(error) => eprintln!("[freehub] workbuddy models inject failed: {error}"),
    }
}

fn wb_models_path() -> PathBuf {
    crate::paths::home_dir().join(".workbuddy").join("models.json")
}

/// 网关 config.json 的路径（注入开关存在这里，是「写哪些模型」的唯一事实来源）
fn gateway_config() -> Value {
    let path = gateway_dir().join("config.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return Value::Null,
    };
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// 平台显示名 / 上下文上限
fn provider_meta(provider: &str) -> (&'static str, u64, &'static str) {
    match provider {
        "orcaterm" => ("OrcaTerm", 1_000_000, "OrcaTerm"),
        "doubao" => ("豆包", 128_000, "Doubao"),
        _ => ("Antigravity", 1_000_000, "Antigravity"),
    }
}

fn prettify(model: &str) -> String {
    let base = model.trim_start_matches("models/");
    base.split(|c| c == '-' || c == '_')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 给「单独注入」的模型起个可读名字。auto 单独处理（自动轮换）。
fn entry_name(provider: &str, model: &str, cfg: &Value) -> String {
    let (vendor, _, _) = provider_meta(provider);
    if model == "auto" {
        return format!("{vendor} · 自动轮换");
    }
    let short = match provider {
        "orcaterm" => ORCATERM_LABELS
            .iter()
            .find(|(id, _)| *id == model)
            .map(|(_, name)| (*name).to_string()),
        "doubao" => cfg
            .pointer("/doubao/catalog/models")
            .and_then(Value::as_array)
            .and_then(|list| {
                list.iter()
                    .find(|m| m.get("id").and_then(Value::as_str) == Some(model))
                    .and_then(|m| m.get("name").and_then(Value::as_str))
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                DOUBAO_LABELS
                    .iter()
                    .find(|(id, _)| *id == model)
                    .map(|(_, name)| (*name).to_string())
            }),
        _ => cfg
            .pointer(&format!("/antigravity/labels/{}", model.replace('~', "~0").replace('/', "~1")))
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
    };
    match short {
        Some(name) => format!("{vendor} · {name}"),
        None => format!("{vendor} · {}", prettify(model)),
    }
}

fn model_entry(provider: &str, model: &str, name: &str) -> Value {
    let (_, max_input, vendor) = provider_meta(provider);
    json!({
        "apiKey": "local",
        "id": format!("{provider}/{model}"),
        "name": name,
        "maxInputTokens": max_input,
        "maxOutputTokens": 8096,
        "supportsImages": false,
        // 网关把上游 reasoning 拼进普通 content，不返回独立的 reasoning_content 帧。
        // 声明 true 会让 WorkBuddy 尝试「关闭思考」并在客户端校验时报错
        // （Current model does not support disabling thinking）。
        "supportsReasoning": false,
        "supportsToolCall": false,
        "url": format!("http://127.0.0.1:{PORT}/v1/chat/completions"),
        "useCustomProtocol": false,
        "vendor": vendor
    })
}

/// 读 config.json 里某个平台的注入计划：`(启用, 要写进 models.json 的模型列表)`
fn inject_plan(cfg: &Value, provider: &str) -> (bool, Vec<String>) {
    // 老配置没有 inject 节点：OrcaTerm / 豆包默认开、Antigravity 登录后才开
    let default_enabled = provider != "antigravity";
    let node = cfg.get("inject").and_then(|v| v.get(provider));
    let enabled = node
        .and_then(|v| v.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(default_enabled);
    let mut models: Vec<String> = node
        .and_then(|v| v.get("inject"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if models.is_empty() {
        models.push("auto".to_string());
    }
    (enabled, models)
}

/// models.json 的真实结构：历史版本是纯数组，新版可能包成 `{"models":[...]}`。
/// 两种都认，且**按原结构写回** —— 把别人的配置格式改坏，WorkBuddy 会直接读不出来。
struct ModelsFile {
    path: PathBuf,
    list: Vec<Value>,
    wrapper: Option<Value>,
}

impl ModelsFile {
    fn load() -> Result<Self, String> {
        // 国内版 ~/.workbuddy，国际版 ~/.workbuddy-ai：存在哪个写哪个，都不存在就建国内版。
        let home = crate::paths::home_dir();
        let candidates = [
            home.join(".workbuddy").join("models.json"),
            home.join(".workbuddy-ai").join("models.json"),
        ];
        let path = candidates
            .iter()
            .find(|path| path.exists())
            .cloned()
            .unwrap_or_else(|| candidates[0].clone());
        if !path.exists() {
            return Ok(Self { path, list: Vec::new(), wrapper: None });
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("读取 {} 失败：{error}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self { path, list: Vec::new(), wrapper: None });
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("{} 不是合法 JSON（{error}），未改动", path.display()))?;
        match value {
            Value::Array(list) => Ok(Self { path, list, wrapper: None }),
            Value::Object(_) => {
                let list = value
                    .get("models")
                    .and_then(Value::as_array)
                    .cloned()
                    .ok_or_else(|| format!("{} 是对象但没有 models 数组，未改动", path.display()))?;
                Ok(Self { path, list, wrapper: Some(value) })
            }
            _ => Err(format!("{} 既不是数组也不是对象，未改动", path.display())),
        }
    }

    fn serialize(&self, list: &[Value]) -> Result<String, String> {
        let value = match &self.wrapper {
            Some(wrapper) => {
                let mut wrapper = wrapper.clone();
                wrapper["models"] = Value::Array(list.to_vec());
                wrapper
            }
            None => Value::Array(list.to_vec()),
        };
        serde_json::to_string_pretty(&value).map_err(|error| format!("序列化失败：{error}"))
    }
}

fn inject_status_path() -> PathBuf {
    gateway_dir().join(".inject-status.json")
}

/// 每次注入的结果落盘：面板「诊断」直接读它 —— 别人机器上不用再猜「为什么模型列表是空的」。
fn write_inject_status(result: &Result<String, String>, path: &Path, count: usize) {
    let payload = json!({
        "at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default(),
        "ok": result.is_ok(),
        "message": match result { Ok(message) => message.clone(), Err(error) => error.clone() },
        "modelsPath": path.display().to_string(),
        "modelsCount": count,
        "appVersion": env!("CARGO_PKG_VERSION"),
    });
    if let Ok(text) = serde_json::to_string_pretty(&payload) {
        let _ = std::fs::write(inject_status_path(), text);
    }
}

/// 把三个平台的模型写进 WorkBuddy 的 models.json（每次都覆盖旧配置）：
/// - 先删掉本应用写入过的全部条目（含已下线 / 已关闭平台的旧 ID），再写当前这一套；
/// - 平台开关关闭 → 该平台一个条目都不写（等于从 WorkBuddy 里清掉）；
/// - 平台下的模型勾选决定写哪些，默认只有「自动轮换」一个；
/// - 其余渠道的模型条目一个不动；每次真正改动前都备份原文件。
pub fn inject_workbuddy_models() -> Result<String, String> {
    match inject_workbuddy_models_inner() {
        Ok((message, path, count)) => {
            write_inject_status(&Ok(message.clone()), &path, count);
            Ok(message)
        }
        Err(error) => {
            write_inject_status(&Err(error.clone()), &wb_models_path(), 0);
            Err(error)
        }
    }
}

fn inject_workbuddy_models_inner() -> Result<(String, PathBuf, usize), String> {
    let file = ModelsFile::load()?;
    let path = file.path.clone();
    let mut models = file.list.clone();
    let before = file.serialize(&models)?;

    // 旧版本写入过不带 TokenHub/ 前缀的 ID、已下线的模型、以及刚被关掉平台的整套条目，
    // 重新配置时一律按前缀 + vendor 清掉（只清本应用写入的，别的渠道不动）。
    let total = models.len();
    let mut kept: Vec<Value> = Vec::with_capacity(total);
    for model in models.drain(..) {
        let id = model.get("id").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        let vendor = model.get("vendor").and_then(Value::as_str).unwrap_or("").trim().to_ascii_lowercase();
        let owned = PROVIDERS.iter().any(|(p, v)| {
            id.starts_with(&format!("{p}/")) || vendor == p.to_ascii_lowercase() || vendor == v.to_ascii_lowercase()
        });
        if !owned {
            kept.push(model);
        }
    }
    let removed = total - kept.len();
    let kept_count = kept.len();
    let mut models = kept;

    let cfg = gateway_config();
    let mut written: Vec<(&str, usize)> = Vec::new();
    let mut disabled: Vec<String> = Vec::new();
    for (provider, _) in PROVIDERS {
        let (enabled, list) = inject_plan(&cfg, provider);
        if !enabled {
            disabled.push(provider_meta(provider).0.to_string());
            continue;
        }
        // 没登录就别写：WorkBuddy 里挂着一个必然 401 的模型，比没有更让人困惑
        if *provider == "antigravity" {
            let logged = cfg
                .pointer("/providers/antigravity/refresh_token")
                .and_then(Value::as_str)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if !logged {
                disabled.push("Antigravity（未登录）".to_string());
                continue;
            }
        }
        let mut seen: Vec<String> = Vec::new();
        for model in list {
            if seen.iter().any(|m| m == &model) {
                continue;
            }
            seen.push(model.clone());
            let name = entry_name(provider, &model, &cfg);
            models.push(model_entry(provider, &model, &name));
        }
        written.push((provider_meta(provider).0, seen.len()));
    }

    let text = file.serialize(&models)?;
    let summary = || {
        written
            .iter()
            .map(|(name, count)| format!("{name} {count} 个"))
            .collect::<Vec<_>>()
            .join(" + ")
    };
    let tail = |prefix: &str| {
        if disabled.is_empty() {
            String::new()
        } else {
            format!("{prefix}{} 未写入", disabled.join("、"))
        }
    };

    if text == before {
        return Ok((
            format!("WorkBuddy 配置已是最新（{}）{}", summary(), tail("；")),
            path,
            models.len(),
        ));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败：{error}", parent.display()))?;
    }
    if path.exists() {
        let backup = path.with_extension(format!(
            "json.bak-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default()
        ));
        let _ = std::fs::copy(&path, &backup);
    }
    std::fs::write(&path, text)
        .map_err(|error| format!("写入 {} 失败：{error}", path.display()))?;
    Ok((
        format!(
            "已覆盖旧配置：清理 {removed} 条旧条目，写入 {}，合计 {} 条{}；其他 {kept_count} 条模型保留。重启 WorkBuddy 生效",
            summary(),
            models.len(),
            tail("，"),
        ),
        path,
        models.len(),
    ))
}


/// WorkBuddy 是否正在运行。models.json 是它启动时读一次的，注入完不重启就看不到新模型 ——
/// 这一条能解释大半「装完了但模型列表还是空的」。
fn workbuddy_running() -> bool {
    #[cfg(target_os = "macos")]
    {
        Command::new("/usr/bin/pgrep")
            .args(["-f", "WorkBuddy.app/Contents/MacOS"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq WorkBuddy.exe", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).contains("WorkBuddy.exe"))
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// 读日志尾部若干行（只读最后 16KB，网关日志可能已经几十 MB）。
fn tail_log(path: &Path, lines: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return String::new(),
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(16 * 1024)));
    let mut buffer = String::new();
    let _ = file.read_to_string(&mut buffer);
    let collected: Vec<&str> = buffer.lines().filter(|line| !line.trim().is_empty()).collect();
    collected[collected.len().saturating_sub(lines)..].join("\n")
}

/// 一键诊断：把「别人机器上为什么用不了」要看的全部信息收成一块，
/// 面板复制出来就能定位问题，不用再一条条反问。
///
/// 关键判据只有四条：网关在不在跑、跑的是哪版脚本、models.json 写没写进去、三个平台登没登录。
pub async fn diagnostics() -> Value {
    let dir = gateway_dir();
    let cfg = gateway_config();
    let state_value = state().await;
    let online = !state_value.get("offline").and_then(Value::as_bool).unwrap_or(false);

    let bundle_version = std::fs::read_to_string(dir.join(".bundle-version"))
        .map(|text| text.trim().to_string())
        .unwrap_or_default();
    let models = ModelsFile::load();
    let (models_path, models_count, models_exists) = match &models {
        Ok(file) => (file.path.display().to_string(), file.list.len(), file.path.exists()),
        Err(_) => (wb_models_path().display().to_string(), 0, false),
    };
    let inject = std::fs::read_to_string(inject_status_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);

    // 平台登录态：网关在线时以它为准（它就是真正在发请求的那份进程）；
    // 离线时退回看 config.json 里有没有凭据，便于判断「是没登录还是网关没起」。
    let non_empty = |pointer: &str| {
        cfg.pointer(pointer)
            .and_then(Value::as_str)
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false)
    };
    let platforms = json!({
        "orcaterm": {
            "cookie": non_empty("/providers/orcaterm/cookie"),
            "loggedIn": state_value.pointer("/orcaterm/loggedIn").cloned().unwrap_or(Value::Null),
        },
        "doubao": {
            "cookie": non_empty("/providers/doubao/cookie"),
            "catalog": state_value.pointer("/doubao/catalog").and_then(Value::as_array).map(|list| list.len()).unwrap_or(0),
            "fallbackModels": cfg.pointer("/doubao/models").and_then(Value::as_array).map(|list| list.len()).unwrap_or(0),
        },
        "antigravity": {
            "refreshToken": non_empty("/providers/antigravity/refresh_token"),
            "quota": state_value.pointer("/antigravity/quota").cloned().unwrap_or(Value::Null),
        },
    });

    #[cfg(target_os = "macos")]
    let node_ok = node_runs(&dir.join("node"));
    #[cfg(target_os = "windows")]
    let node_ok = {
        use std::os::windows::process::CommandExt;
        let exe = dir.join("node.exe");
        exe.exists()
            && Command::new(&exe)
                .arg("-v")
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let node_ok = false;

    let providers_with_key = cfg
        .get("providers")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter(|(_, value)| {
                    value.get("apiKey").and_then(Value::as_str).map(|key| !key.trim().is_empty()).unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);

    json!({
        "appVersion": env!("CARGO_PKG_VERSION"),
        "workbuddyRunning": workbuddy_running(),
        "gateway": {
            "url": format!("http://127.0.0.1:{PORT}"),
            "online": online,
            "runningVersion": state_value.get("version").cloned().unwrap_or(Value::Null),
            "bundleVersion": bundle_version,
            "uptimeSec": state_value.get("uptimeSec").cloned().unwrap_or(Value::Null),
            "queueSize": state_value.get("queueSize").cloned().unwrap_or(Value::Null),
            "skipped": state_value.get("skipped").cloned().unwrap_or(Value::Null),
            "message": state_value.get("message").cloned().unwrap_or(Value::Null),
            "nodeRunnable": node_ok,
        },
        "paths": {
            "dir": dir.display().to_string(),
            "config": dir.join("config.json").display().to_string(),
            "models": models_path,
            "modelsExists": models_exists,
            "modelsCount": models_count,
            "plist": plist_path().display().to_string(),
        },
        "providersWithKey": providers_with_key,
        "inject": inject,
        "platforms": platforms,
        "logs": {
            "stderr": tail_log(&dir.join("stderr.log"), 20),
            "stdout": tail_log(&dir.join("stdout.log"), 20),
            // 登录器是 detached 起的，崩了只会写这里（Chrome 不在 / node 被杀 / 二维码没出来）
            "doubaoLogin": tail_log(&dir.join("doubao-login.log"), 15),
            "orcatermLogin": tail_log(&dir.join("orcaterm-login.log"), 15),
        },
    })
}

fn endpoint() -> String {
    format!("http://127.0.0.1:{PORT}/admin/api")
}

fn client(timeout: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .build()
        .map_err(|error| format!("构建 HTTP 客户端失败：{error}"))
}

/// 只允许这几个管理路径，别的一律拒绝：这个函数是唯一的出站口。
fn allowed(path: &str) -> bool {
    matches!(
        path,
        "/state" | "/provider" | "/delete" | "/tier" | "/test" | "/inject"
    )
}

async fn call(method: reqwest::Method, path: &str, body: Option<Value>, timeout: u64) -> Result<Value, String> {
    if !allowed(path) {
        return Err(format!("不允许的网关路径：{path}"));
    }
    let mut request = client(timeout)?
        .request(method, format!("{}{}", endpoint(), path))
        .header("Accept", "application/json");
    if let Some(payload) = body {
        request = request.json(&payload);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("网关无响应：{error}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|error| format!("网关响应不是 JSON：{error}"))?;
    if !status.is_success() {
        // 网关的 4xx 一律带 {ok:false,error}：把原文透给前端，别改写成笼统的一句话
        let reason = parsed
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("未知错误")
            .to_string();
        return Err(reason);
    }
    Ok(parsed)
}

/// 面板状态。网关不可达不是异常，是「没装/没起」这个正常状态。
pub async fn state() -> Value {
    match call(reqwest::Method::GET, "/state", None, 8).await {
        Ok(value) => value,
        Err(message) => json!({ "ok": false, "offline": true, "message": message }),
    }
}

/// 三个平台的注入开关（enabled / rotate / inject），面板用来初始化勾选状态。
/// 关掉网关时返回空对象，面板按默认值渲染。
pub async fn inject_state() -> Value {
    match call(reqwest::Method::GET, "/state", None, 8).await {
        Ok(value) => value.get("inject").cloned().unwrap_or_else(|| json!({})),
        Err(_) => json!({}),
    }
}

/// 改某个平台的注入开关。只写网关 config.json，写 models.json 由
/// `inject_workbuddy_models()` 接着做（前端拿到结果后会再调一次刷新）。
pub async fn set_inject(
    provider: &str,
    enabled: Option<bool>,
    rotate: Option<Value>,
    inject: Option<Value>,
) -> Result<Value, String> {
    let mut body = json!({ "provider": provider });
    if let Some(value) = enabled {
        body["enabled"] = json!(value);
    }
    if let Some(value) = rotate {
        body["rotate"] = value;
    }
    if let Some(value) = inject {
        body["inject"] = value;
    }
    call(reqwest::Method::POST, "/inject", Some(body), 20).await
}

pub async fn save(name: &str, patch: Value, create: bool) -> Result<Value, String> {    if name.trim().is_empty() {
        return Err("渠道标识不能为空".to_string());
    }
    call(
        reqwest::Method::POST,
        "/provider",
        Some(json!({ "name": name, "patch": patch, "create": create })),
        15,
    )
    .await
}

pub async fn remove(name: &str) -> Result<Value, String> {
    call(reqwest::Method::POST, "/delete", Some(json!({ "name": name })), 15).await
}

pub async fn tier(tier: &str) -> Result<Value, String> {
    call(reqwest::Method::POST, "/tier", Some(json!({ "tier": tier })), 15).await
}

/// 真发一次请求量延迟。上游慢模型可能 60 秒以上，预算给到 150 秒。
pub async fn test(name: &str, model: Option<String>) -> Result<Value, String> {
    call(
        reqwest::Method::POST,
        "/test",
        Some(json!({ "name": name, "model": model })),
        150,
    )
    .await
}

/// 拉起网关：先确保内置资源已解包、服务已注册，再 kickstart。
/// 新电脑上即使从未手动装过网关，这一步也能自愈。
pub async fn start(resource_dir: &Path) -> Result<Value, String> {
    #[cfg(target_os = "macos")]
    {
        let _ = ensure_installed(resource_dir)?;
        launchctl_bootstrap()?;
        launchctl_kickstart()?;
        // KeepAlive 拉起有节流，轮询到 /state 能通为止，最多 12 秒
        for _ in 0..24 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if call(reqwest::Method::GET, "/state", None, 3).await.is_ok() {
                return Ok(json!({ "ok": true }));
            }
        }
        Err("已发起启动，但网关 12 秒内没有响应，看 ~/.wb-checkin-gateway/gateway.log".to_string())
    }
    #[cfg(target_os = "windows")]
    {
        let changed = ensure_installed_windows(resource_dir)?;
        if gateway_alive() && changed {
            kill_gateway();
        }
        if !gateway_alive() {
            spawn_gateway()?;
        }
        // 进程拉起有启动开销（要读 config、扫模型），轮询到 /state 通为止，最多 12 秒
        let mut ready = false;
        for _ in 0..24 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if call(reqwest::Method::GET, "/state", None, 3).await.is_ok() {
                ready = true;
                break;
            }
        }
        if ready {
            Ok(json!({ "ok": true }))
        } else {
            Err(format!(
                "已发起启动，但网关 12 秒内没有响应，看 {}\\gateway.log",
                gateway_dir().display()
            ))
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = resource_dir;
        Err("本平台暂不支持自动启动网关".to_string())
    }
}

/// 打开「申请 key」这类外部页面（各家平台的注册 / 控制台地址）。
///
/// 不能复用 `open_link`：那条命令只放行 `workbuddy://` 与 `workbuddy.cn` 站内地址
/// （签到跳转用），拿它开 `https://dash.cloudflare.com/...` 会被拒，而且拒绝是异步的，
/// 界面上就是「点了没反应」。这里只放行 http/https —— 不给 `file://` 或自定义 scheme
/// 开口子，链接本身来自网关的 config.json，不是用户随手粘的任意命令。
pub fn open(url: &str) -> Result<(), String> {
    let target = url.trim();
    if target.is_empty() {
        return Err("没有可打开的地址".to_string());
    }
    if !(target.starts_with("https://") || target.starts_with("http://")) {
        return Err(format!("只允许打开 http/https 链接：{target}"));
    }
    crate::open_external(target)
}
