//! 国际版 WorkBuddy AI 摆渡（WB AI Relay）。
//!
//! 定位：把**国际版账号**的模型额度摆渡给国内版 WorkBuddy 用。国内版账户不动、
//! 不占积分，模型下拉里多一条走自己国际账号的模型。
//!
//! 与 `ferry` 的区别：ferry 靠内置 Node sidecar 去驱动 opencode，本模块**全部是本进程内的
//! Rust 实现** —— 不依赖任何外部运行时，所以在 `.app` 里能直接分发，Windows 侧同样可跑。
//!
//! ## 协议（2026-10-29 实测，非推测，改之前先读）
//!
//! 国际版**没有公开开发者 API**，只有私有接口。整套流程可以自己走一遍，**不需要碰客户端
//! 的 `keyblob`**（那是 Keychain 包装的加密登录态，本模块一律不读）：
//!
//! ```text
//! POST {API}/v2/plugin/auth/state?platform=web   → { state, authUrl }
//!      浏览器打开 authUrl，用户登录
//! GET  {API}/v2/plugin/auth/token?state=...      → accessToken / refreshToken
//! POST {API}/v2/plugin/auth/token/refresh        → 头 X-Refresh-Token
//! POST {API}/v2/chat/completions                 → OpenAI 格式对话
//! ```
//!
//! `/v2/plugin` 这个前缀是 asar 里 `prefixPath` 常量，也是整件事唯一的坎：
//! 少了它 `/v2/auth/state` 会 404。
//!
//! ## 上游的三条硬性要求（少一条就是 400）
//!
//! 1. **只接受 `stream:true`**，非流式返回 `11101`。所以本模块对上游永远发流式，
//!    客户端要非流式时由 `aggregate()` 把 SSE 聚合成标准 `chat.completion`。
//! 2. **`messages[0]` 必须是 `system`**，否则 `11128 first message is not system prompt`。
//! 3. **必须带整套客户端指纹头**（UA 双写、`X-Conversation-*`、`acp-connection-id`、
//!    `traceparent`/`b3` 等），否则 `11128 request illegal`（提示「不是官方客户端发出的」）。
//!
//! ## 安全口径
//!
//! 转发口只绑回环。token 由本进程独立 OAuth 获得（与官方客户端的登录态互不影响，
//! 不会把它挤下线），落盘 0600。开关默认关闭，只能由用户显式启动。

use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use crate::paths;

const PORT: u16 = 8799;
/// 国际版后端。国内域名直连最稳，见 `client()` 的说明。
const API: &str = "https://www.workbuddy.ai";
/// 请求头里伪装成官方客户端用的版本号。客户端升级后可同步改，不影响功能。
const IDE_VERSION: &str = "5.7.6";
/// 上游强制要求首条消息是 system；客户端没给就补这一条。
const DEFAULT_SYSTEM: &str =
    "You are WorkBuddy, a helpful AI assistant. Answer in the user's language.";
/// 登录会话有效期。用户在浏览器里登录一般 1 分钟内完成。
const LOGIN_TIMEOUT_SECONDS: i64 = 300;

/// 内置兜底清单：读不到国际版客户端的模型缓存时才用它，免得面板空着。
/// 真正的清单以客户端缓存里的产品配置为准，见 `catalog()`。
const BUILTIN: [&str; 9] = [
    "deepseek-v4.1-flash",
    "deepseek-v4.1-flash-sg",
    "deepseek-v4-pro",
    "hy3",
    "glm-5.3",
    "glm-5.3-flash",
    "minimax-m3.1-flash",
    "gpt-6-luna",
    "gpt-5.6-luna",
];

/// 导入到 models.json 的 id 前缀。带上它，国际版的 `glm-5.3` 就不会和你自己
/// 配的 `glm-5.3`（别的转发口）撞 id；转发时由 `strip_client_prefix` 再剥掉。
const CLIENT_PREFIX: &str = "wbai/";

/// 我们自己写进 models.json 的条目靠这个字段认领，只增删自己放的，用户手写的一条不动。
const OWNER_FIELD: &str = "wbaiOwner";
const OWNER: &str = "wbai-v1";

/// 探测结论缓存多久。模型能不能用是账号态问题，10 分钟内不会变。
const PROBE_TTL_MS: i64 = 10 * 60 * 1000;

// ---------------------------------------------------------------------------
// 目录与文件
// ---------------------------------------------------------------------------

pub fn data_dir() -> PathBuf {
    paths::data_dir().join("wbai")
}

fn token_file() -> PathBuf {
    data_dir().join("token.json")
}

fn enabled_marker() -> PathBuf {
    data_dir().join("enabled")
}

fn log_file() -> PathBuf {
    data_dir().join("wbai.log")
}

fn ensure_dir() -> Result<(), String> {
    paths::ensure_private_dir(&data_dir()).map_err(|error| format!("创建目录失败：{error}"))
}

/// 写一行日志。取不到文件就静默 —— 日志写不进去不该让功能失败。
fn log_line(text: &str) {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file())
    {
        let _ = writeln!(file, "[{stamp}] {text}");
    }
}

// ---------------------------------------------------------------------------
// HTTP 客户端
// ---------------------------------------------------------------------------

/// 访问上游用的客户端。
///
/// **必须 `no_proxy()`**：reqwest 默认吃 `HTTP(S)_PROXY` 或系统代理，而本机系统代理指向
/// 一个国外出口，走它时带 tools 的大请求和 SSE 长连接会被 TLS 层直接切断，症状是
/// `<urlopen error EOF occurred in violation of protocol>` / 「502 upstream unreachable」。
/// 这个坑在 Python 版上踩过一次，别退回去。
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(600))
            .build()
            .expect("构建上游客户端失败")
    })
}

/// 短超时客户端：只用于登录/刷新这类元信息接口。
fn meta_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(25))
            .build()
            .expect("构建登录客户端失败")
    })
}

// ---------------------------------------------------------------------------
// 客户端指纹头
// ---------------------------------------------------------------------------

/// 32 位十六进制随机串（trace/span 用）。用 uuid 去掉横线，不额外引 rand。
fn hex32() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn hex16() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// 构造一整套官方客户端请求头。
///
/// 这些头每一个都是实测出来的：只带 `X-Product` / `X-IDE-*` 而缺 UA、`X-Conversation-*`、
/// trace 系列时，上游返回 `11128 request illegal`，displayTips 明说「可能不是官方客户端发出的」。
/// 其中 **User-Agent 也是判据之一**，别以为换掉无所谓。
fn upstream_headers(token: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    let trace = hex32();
    let span = hex16();
    let parent = hex16();
    let ua = format!("WorkBuddy/{IDE_VERSION} WorkBuddy/{IDE_VERSION}");

    let pairs: Vec<(&str, String)> = vec![
        ("Authorization", format!("Bearer {token}")),
        ("User-Agent", ua),
        ("Accept", "application/json".into()),
        ("x-requested-with", "XMLHttpRequest".into()),
        ("X-Product", "SaaS".into()),
        ("X-IDE-Type", "WorkBuddy".into()),
        ("X-IDE-Name", "WorkBuddy".into()),
        ("X-IDE-Version", IDE_VERSION.into()),
        ("X-Private-Data", "true".into()),
        ("X-Conversation-ID", uuid::Uuid::new_v4().to_string()),
        ("X-Conversation-Request-ID", uuid::Uuid::new_v4().to_string()),
        ("acp-connection-id", uuid::Uuid::new_v4().to_string()),
        ("X-Conversation-Message-ID", uuid::Uuid::new_v4().to_string()),
        ("X-Request-ID", uuid::Uuid::new_v4().to_string()),
        ("X-Agent-Intent", "craft".into()),
        ("X-Agent-Purpose", "conversation".into()),
        ("x-codebuddy-request", "1".into()),
        ("traceparent", format!("00-{trace}-{span}-01")),
        ("b3", format!("{trace}-{span}-1-{parent}")),
        ("X-B3-TraceId", trace),
        ("X-B3-ParentSpanId", parent),
        ("X-B3-SpanId", span),
        ("X-B3-Sampled", "1".into()),
        ("Accept-Encoding", "identity".into()),
    ];

    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

/// 登录/刷新这类接口只需要最基础的几项，多带无益。
fn meta_headers(extra: Option<(&str, &str)>) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut headers = HeaderMap::new();
    let mut pairs: Vec<(String, String)> = vec![
        ("X-Product".into(), "SaaS".into()),
        ("X-IDE-Type".into(), "WorkBuddy".into()),
        ("X-IDE-Name".into(), "WorkBuddy".into()),
        ("X-IDE-Version".into(), IDE_VERSION.into()),
        (
            "User-Agent".into(),
            format!("WorkBuddy/{IDE_VERSION} WorkBuddy/{IDE_VERSION}"),
        ),
    ];
    if let Some((name, value)) = extra {
        pairs.push((name.to_string(), value.to_string()));
    }
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

// ---------------------------------------------------------------------------
// Token
// ---------------------------------------------------------------------------

struct Token {
    access: String,
    refresh: Option<String>,
    account: Option<String>,
}

fn read_token() -> Option<Token> {
    let text = fs::read_to_string(token_file()).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let access = value
        .get("accessToken")
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    if access.is_empty() {
        return None;
    }
    Some(Token {
        access,
        refresh: value
            .get("refreshToken")
            .and_then(Value::as_str)
            .map(str::to_string),
        account: value
            .get("account")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn save_token(access: &str, refresh: Option<&str>, account: Option<&str>) -> Result<(), String> {
    ensure_dir()?;
    let value = json!({
        "accessToken": access,
        "refreshToken": refresh,
        "account": account,
        "savedAt": paths::now_ms(),
    });
    let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    // write_private 会处理 0600 与跨平台差异。
    paths::write_private(&token_file(), &text).map_err(|error| format!("保存登录态失败：{error}"))
}

/// 把 JWT 里能公开读的字段解出来（只用于显示账号，不验签）。
fn claims_of(jwt: &str) -> Value {
    use base64::Engine;
    let Some(payload) = jwt.split('.').nth(1) else {
        return Value::Null;
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return Value::Null;
    };
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn account_of(jwt: &str) -> Option<String> {
    let claims = claims_of(jwt);
    claims
        .get("email")
        .or_else(|| claims.get("preferred_username"))
        .or_else(|| claims.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 刷新 token。串行化，避免并发请求同时刷新互相作废。
///
/// 用 tokio 的 Mutex 而不是 std 的：`std::sync::MutexGuard` 不是 `Send`，一旦跨 await
/// 持有，整个 handler 的 future 就变不成 `Send`，`tokio::spawn` 直接编译不过。
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn refresh_token() -> Result<String, String> {
    let _guard = REFRESH_LOCK.lock().await;
    let Some(token) = read_token() else {
        return Err("还没有登录".into());
    };
    let Some(rt) = token.refresh else {
        return Err("没有 refreshToken，请重新登录".into());
    };

    let response = meta_client()
        .post(format!("{API}/v2/plugin/auth/token/refresh"))
        .headers(meta_headers(Some(("X-Refresh-Token", &rt))))
        .json(&json!({}))
        .send()
        .await
        .map_err(|error| format!("刷新请求失败：{error}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("刷新返回 {status}：{}", text.chars().take(200).collect::<String>()));
    }
    let value: Value = serde_json::from_str(&text).map_err(|_| "刷新响应不是 JSON".to_string())?;
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    let access = data
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "刷新没有拿到 accessToken：{}",
                text.chars().take(200).collect::<String>()
            )
        })?
        .to_string();
    let new_refresh = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::to_string);
    save_token(
        &access,
        new_refresh.as_deref().or(Some(rt.as_str())),
        token.account.as_deref(),
    )?;
    log_line("token 已刷新");
    Ok(access)
}

// ---------------------------------------------------------------------------
// 模型目录（读国际版客户端缓存里的产品配置）
// ---------------------------------------------------------------------------

/// 一个模型。`credits` 是面板直接显示的短文案：「免费」/「0.29×」/「—」。
#[derive(Clone)]
struct ModelEntry {
    id: String,
    name: String,
    credits: String,
    free: bool,
    input: i64,
    output: i64,
    images: bool,
    tools: bool,
    reasoning: bool,
}

#[derive(Clone)]
struct Catalog {
    models: Vec<ModelEntry>,
    source: String,
}

/// 国际版客户端把产品配置缓存在这里。里面有 `models` 数组，每项带 `credits`
/// （积分倍率）与能力位 —— 这就是「哪些模型免费」的唯一可靠来源。
///
/// 不硬编码：用户换套餐、官方上下架模型，缓存会跟着变；硬编码的清单会静默过期。
fn catalog_dir() -> PathBuf {
    paths::home_dir().join(".workbuddy-ai/cache")
}

fn credits_label(item: &Value) -> (String, bool) {
    let raw = item
        .get("credits")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if raw.is_empty() {
        return ("—".into(), false);
    }
    // "x0.00" / "x0.29 credits" / "x10.00 credits" 三种写法都见过。
    let number: String = raw
        .trim_start_matches('x')
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    match number.parse::<f64>() {
        Ok(value) if value == 0.0 => ("免费".into(), true),
        Ok(value) => (format!("{value:.2}×"), false),
        Err(_) => (raw.to_string(), false),
    }
}

/// 扫缓存目录建目录。不联网、不解密，纯读客户端已经落盘的明文配置。
fn catalog() -> Catalog {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(catalog_dir())
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| name.starts_with("acc-product-config") && name.ends_with(".json"))
                        .unwrap_or(false)
                })
                .filter_map(|path| {
                    let modified = fs::metadata(&path).ok()?.modified().ok()?;
                    Some((modified, path))
                })
                .collect()
        })
        .unwrap_or_default();
    // 新的先读：同一模型出现在多份配置里时，新的那份说了算。
    files.sort_by(|a, b| b.0.cmp(&a.0));

    let mut models: Vec<ModelEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (_, path) in &files {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(list) = value.get("models").and_then(Value::as_array) else {
            continue;
        };
        for item in list {
            let Some(id) = item
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            // 没有上下文长度的是图片/视频模型，走不了对话接口，不列出来。
            let Some(input) = item.get("maxInputTokens").and_then(Value::as_i64) else {
                continue;
            };
            if !seen.insert(id.to_string()) {
                continue;
            }
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string();
            let (credits, free) = credits_label(item);
            models.push(ModelEntry {
                id: id.to_string(),
                name,
                credits,
                free,
                input,
                output: item.get("maxOutputTokens").and_then(Value::as_i64).unwrap_or(0),
                images: item
                    .get("supportsImages")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                tools: item
                    .get("supportsToolCall")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                reasoning: item
                    .get("supportsReasoning")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }

    let source = if models.is_empty() {
        models = BUILTIN
            .iter()
            .map(|id| ModelEntry {
                id: (*id).to_string(),
                name: (*id).to_string(),
                credits: "—".into(),
                free: false,
                input: 0,
                output: 0,
                images: false,
                tools: true,
                reasoning: false,
            })
            .collect();
        "内置清单（没找到国际版客户端的模型缓存，打开一次国际版即可刷新）".to_string()
    } else {
        format!(
            "来自国际版客户端缓存 · {} 个模型（读自 {} 份产品配置）",
            models.len(),
            files.len()
        )
    };

    // 免费的排前面：用户要的就是它们，翻半天找一个免费模型不合理。
    models.sort_by(|a, b| {
        b.free
            .cmp(&a.free)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Catalog { models, source }
}

/// 面板 2.5 秒刷一次状态，每次重扫 + 重解析 700KB JSON 是不可接受的，缓存 30 秒。
fn catalog_cached() -> Catalog {
    static CACHE: OnceLock<Mutex<Option<(i64, Catalog)>>> = OnceLock::new();
    let cell = CACHE.get_or_init(|| Mutex::new(None));
    let now = paths::now_ms();
    if let Some((at, catalog)) = cell
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        if now - *at < 30_000 {
            return catalog.clone();
        }
    }
    let fresh = catalog();
    *cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((now, fresh.clone()));
    fresh
}

// ---------------------------------------------------------------------------
// 选择：哪些模型要导入
// ---------------------------------------------------------------------------

fn selection_file() -> PathBuf {
    data_dir().join("selection.json")
}

/// 读用户的选择。`None` = 从没选过（用默认：免费的都选上），
/// `Some([])` = 用户全关掉了，这是合法状态，不能当成「没选过」。
fn read_selection() -> Option<Vec<String>> {
    let text = fs::read_to_string(selection_file()).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let array = value.get("selected")?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

fn write_selection(selected: &[String]) -> Result<(), String> {
    ensure_dir()?;
    let value = json!({ "selected": selected, "savedAt": paths::now_ms() });
    let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    paths::write_private(&selection_file(), &text).map_err(|error| format!("保存选择失败：{error}"))
}

/// 生效的选择：存过就用存的（只保留目录里还在的，下架的模型不留幽灵条目），
/// 没存过就默认选中免费的。
fn effective_selection(models: &[ModelEntry]) -> Vec<String> {
    let known: std::collections::HashSet<&str> = models.iter().map(|m| m.id.as_str()).collect();
    let default_free = || -> Vec<String> {
        models
            .iter()
            .filter(|m| m.free)
            .map(|m| m.id.clone())
            .collect()
    };
    match read_selection() {
        Some(list) => {
            let kept: Vec<String> = list
                .into_iter()
                .filter(|id| known.contains(id.as_str()))
                .collect();
            // 存过的模型全被下架了：当作没选过，回到默认，否则面板会显示 0 个勾选
            // 而用户根本没动过。
            if kept.is_empty() {
                default_free()
            } else {
                kept
            }
        }
        None => default_free(),
    }
}

// ---------------------------------------------------------------------------
// 探测：模型到底能不能用
// ---------------------------------------------------------------------------

fn probe_file() -> PathBuf {
    data_dir().join("probe.json")
}

fn read_probe_cache() -> HashMap<String, Value> {
    fs::read_to_string(probe_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .map(|object| object.into_iter().collect())
        .unwrap_or_default()
}

fn write_probe_cache(map: &HashMap<String, Value>) {
    if ensure_dir().is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(map) {
        let _ = paths::write_private(&probe_file(), &text);
    }
}

/// 把上游错误归成面板能直说的几档。
fn classify_probe_error(code: Option<i64>, message: &str) -> (&'static str, String) {
    let lowered = message.to_lowercase();
    if code == Some(11158) {
        return ("not_allowed", "账号类型不支持这个模型".into());
    }
    if code == Some(11128) {
        return ("illegal", "请求被判非法（客户端指纹不完整）".into());
    }
    if lowered.contains("not allowed for user type") {
        return ("not_allowed", "账号类型不支持这个模型".into());
    }
    if lowered.contains("not exist") || lowered.contains("not found") {
        return ("not_found", "上游没有这个模型".into());
    }
    if lowered.contains("quota") || lowered.contains("credit") {
        return ("quota", "额度不足".into());
    }
    if lowered.contains("rate limit") || lowered.contains("too many") {
        return ("rate_limit", "被上游限流".into());
    }
    ("error", message.chars().take(200).collect())
}

/// 单模型探活：发一个最小流式请求，读到第一个数据块就算通。
async fn probe_one(model: &str) -> Value {
    let start = std::time::Instant::now();
    let Some(token) = read_token() else {
        return json!({ "ok": false, "status": "no_login", "error": "还没有登录国际版账号", "at": paths::now_ms() });
    };
    let body = json!({
        "model": model,
        "stream": true,
        "max_tokens": 16,
        "messages": [
            { "role": "system", "content": DEFAULT_SYSTEM },
            { "role": "user", "content": "ping" },
        ],
    });

    let mut access = token.access;
    for attempt in 0..2 {
        let sent = client()
            .post(format!("{API}/v2/chat/completions"))
            .headers(upstream_headers(&access))
            .json(&body)
            .send()
            .await;

        let mut response = match sent {
            Ok(response) => response,
            Err(error) => {
                if attempt == 0 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
                return json!({
                    "ok": false, "status": "unreachable",
                    "error": format!("连不上上游：{error}"),
                    "ms": start.elapsed().as_millis() as i64, "at": paths::now_ms(),
                });
            }
        };

        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) && attempt == 0 {
            if let Ok(fresh) = refresh_token().await {
                access = fresh;
                continue;
            }
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let code = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| value.get("code").and_then(Value::as_i64));
            let (kind, message) = classify_probe_error(code, &text);
            return json!({
                "ok": false, "status": kind, "error": message,
                "ms": start.elapsed().as_millis() as i64, "at": paths::now_ms(),
            });
        }

        // 200 也可能是空流，必须读到第一块才算真的能出字。
        match tokio::time::timeout(Duration::from_secs(40), response.chunk()).await {
            Ok(Ok(_)) => {
                return json!({
                    "ok": true, "status": "ok", "error": Value::Null,
                    "ms": start.elapsed().as_millis() as i64, "at": paths::now_ms(),
                });
            }
            Ok(Err(error)) => {
                return json!({
                    "ok": false, "status": "error",
                    "error": format!("读取响应失败：{error}"),
                    "ms": start.elapsed().as_millis() as i64, "at": paths::now_ms(),
                });
            }
            Err(_) => {
                return json!({
                    "ok": false, "status": "timeout", "error": "40 秒内没有产出",
                    "ms": start.elapsed().as_millis() as i64, "at": paths::now_ms(),
                });
            }
        }
    }

    json!({
        "ok": false, "status": "error", "error": "探测失败",
        "ms": start.elapsed().as_millis() as i64, "at": paths::now_ms(),
    })
}

/// 探测一批模型。`scope` 为 `"all"` 时测整个目录，否则只测已勾选的。
///
/// 并发 4：国际版对同一账号的并发比较宽松，但没必要压满 —— 用户等 10 个模型
/// 逐个探测等不起，并发 4 已经能把 40 秒的串行压到十秒级。
pub async fn probe_models(scope: &str, force: bool) -> Value {
    let catalog = catalog_cached();
    let targets: Vec<String> = if scope == "all" {
        catalog.models.iter().map(|m| m.id.clone()).collect()
    } else {
        effective_selection(&catalog.models)
    };
    if targets.is_empty() {
        return json!({ "ok": false, "error": "没有要检测的模型：先在列表里打开几个" });
    }

    let now = paths::now_ms();
    let mut cache = read_probe_cache();
    let pending: Vec<String> = targets
        .iter()
        .filter(|id| {
            if force {
                return true;
            }
            match cache.get(*id).and_then(|value| value.get("at")).and_then(Value::as_i64) {
                Some(at) => now - at > PROBE_TTL_MS,
                None => true,
            }
        })
        .cloned()
        .collect();

    let started = std::time::Instant::now();
    if !pending.is_empty() {
        use futures_util::StreamExt;
        let results: Vec<(String, Value)> = futures_util::stream::iter(pending.into_iter())
            .map(|id| async move {
                let value = probe_one(&id).await;
                (id, value)
            })
            .buffer_unordered(4)
            .collect()
            .await;
        for (id, value) in results {
            if value.get("ok").and_then(Value::as_bool) == Some(true) {
                log_line(&format!("探测通过 {id}"));
            } else {
                log_line(&format!(
                    "探测失败 {id}：{}",
                    value.get("error").and_then(Value::as_str).unwrap_or("")
                ));
            }
            cache.insert(id, value);
        }
        write_probe_cache(&cache);
    }

    let mut out = serde_json::Map::new();
    for id in &targets {
        if let Some(value) = cache.get(id) {
            out.insert(id.clone(), value.clone());
        }
    }
    json!({
        "ok": true,
        "results": Value::Object(out),
        "probed": targets.len(),
        "taken": started.elapsed().as_millis() as i64,
    })
}

// ---------------------------------------------------------------------------
// 登录（state → authUrl → 轮询）
// ---------------------------------------------------------------------------

struct Login {
    state: String,
    auth_url: String,
    expires_at_ms: i64,
    done: bool,
    result: Option<Value>,
    error: Option<String>,
}

static LOGINS: OnceLock<Mutex<HashMap<String, Login>>> = OnceLock::new();

fn logins() -> &'static Mutex<HashMap<String, Login>> {
    LOGINS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_logins() -> std::sync::MutexGuard<'static, HashMap<String, Login>> {
    logins().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 第一步：申请登录状态，拿到 `authUrl`。
pub async fn start_login() -> Result<Value, String> {
    let response = meta_client()
        .post(format!("{API}/v2/plugin/auth/state?platform=web"))
        .headers(meta_headers(None))
        .json(&json!({}))
        .send()
        .await
        .map_err(|error| format!("申请登录状态失败：{error}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("申请登录状态返回 {status}：{}", text.chars().take(200).collect::<String>()));
    }
    let value: Value = serde_json::from_str(&text).map_err(|_| "登录响应不是 JSON".to_string())?;
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    let state = data
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("没有拿到 state：{}", text.chars().take(200).collect::<String>()))?
        .to_string();
    let auth_url = data
        .get("authUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| "没有拿到 authUrl".to_string())?
        .to_string();

    // authUrl 必须是官方域名，避免把用户带去别处。
    if !auth_url.starts_with("https://www.workbuddy.ai/") {
        return Err(format!("登录地址不在官方域名内，已拒绝：{auth_url}"));
    }

    let login_id = uuid::Uuid::new_v4().to_string();
    lock_logins().insert(
        login_id.clone(),
        Login {
            state,
            auth_url: auth_url.clone(),
            expires_at_ms: paths::now_ms() + LOGIN_TIMEOUT_SECONDS * 1000,
            done: false,
            result: None,
            error: None,
        },
    );

    Ok(json!({
        "loginId": login_id,
        "authUrl": auth_url,
        "expiresIn": LOGIN_TIMEOUT_SECONDS,
    }))
}

/// 打开登录页。只接受本进程自己申请的地址，前端无法诱导打开任意 URL。
pub fn open_login_page(login_id: &str) -> Result<(), String> {
    let url = {
        let map = lock_logins();
        let session = map
            .get(login_id)
            .ok_or_else(|| "登录会话不存在或已取消".to_string())?;
        session.auth_url.clone()
    };
    crate::open_external(&url).map_err(|error| format!("打开浏览器失败：{error}"))
}

pub fn cancel_login(login_id: &str) -> bool {
    lock_logins().remove(login_id).is_some()
}

/// 第二步：轮询一次。前端每 2 秒调一次，与国内版登录（`oauth::poll`）同一套交互。
pub async fn poll_login(login_id: &str) -> Value {
    let state = {
        let mut map = lock_logins();
        let Some(session) = map.get_mut(login_id) else {
            return json!({
                "done": true, "status": "error",
                "error": "登录会话不存在或已取消", "expiresIn": 0,
            });
        };
        if session.done {
            return snapshot(session);
        }
        if paths::now_ms() > session.expires_at_ms {
            session.done = true;
            session.error = Some("登录超时，请重新发起".into());
            return snapshot(session);
        }
        session.state.clone()
    };

    let result = fetch_login_token(&state).await;

    let mut map = lock_logins();
    let Some(session) = map.get_mut(login_id) else {
        return json!({ "done": true, "status": "error", "error": "登录会话已取消", "expiresIn": 0 });
    };
    match result {
        Ok(Fetched::Pending) => {
            session.done = false;
            snapshot(session)
        }
        Ok(Fetched::Token(access, refresh, account)) => {
            session.done = true;
            session.result = Some(json!({
                "access": access, "refresh": refresh, "account": account,
            }));
            snapshot(session)
        }
        Err(error) => {
            // 单次网络抖动不判死，只有明确失败才结束会话。
            session.done = true;
            session.error = Some(error);
            snapshot(session)
        }
    }
}

enum Fetched {
    Pending,
    Token(String, Option<String>, Option<String>),
}

async fn fetch_login_token(state: &str) -> Result<Fetched, String> {
    let response = meta_client()
        .get(format!("{API}/v2/plugin/auth/token?state={state}"))
        .headers(meta_headers(None))
        .send()
        .await
        .map_err(|error| format!("轮询失败：{error}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("轮询返回 {status}：{}", text.chars().take(200).collect::<String>()));
    }
    let value: Value = serde_json::from_str(&text).map_err(|_| "轮询响应不是 JSON".to_string())?;
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let message = value
        .get("msg")
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();

    // 11217 = 还在等用户登录，属于正常中间态。
    if code == 11217 || message.contains("login ing") {
        return Ok(Fetched::Pending);
    }

    let data = value.get("data").cloned().unwrap_or(Value::Null);
    let Some(access) = data.get("accessToken").and_then(Value::as_str) else {
        return Err(format!(
            "登录未完成：{}",
            text.chars().take(200).collect::<String>()
        ));
    };
    let access = access.to_string();
    let refresh = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::to_string);
    let account = account_of(&access);
    save_token(&access, refresh.as_deref(), account.as_deref())?;
    log_line(&format!("登录成功：{}", account.clone().unwrap_or_else(|| "未知账号".into())));
    Ok(Fetched::Token(access, refresh, account))
}

fn snapshot(session: &Login) -> Value {
    let expires_in = ((session.expires_at_ms - paths::now_ms()) / 1000).max(0);
    if session.done {
        if let Some(error) = &session.error {
            return json!({
                "done": true, "status": "error", "error": error, "expiresIn": 0,
            });
        }
        let result = session.result.clone().unwrap_or(Value::Null);
        return json!({
            "done": true, "status": "ok",
            "account": result.get("account").cloned().unwrap_or(Value::Null),
            "expiresIn": 0,
        });
    }
    json!({ "done": false, "status": "pending", "expiresIn": expires_in })
}

/// 当前登录态（不联网）。
pub fn account() -> Value {
    match read_token() {
        Some(token) => {
            let claims = claims_of(&token.access);
            let exp = claims.get("exp").and_then(Value::as_i64);
            let now = paths::now_ms() / 1000;
            json!({
                "loggedIn": true,
                "account": token.account,
                "expiresAt": exp.map(|value| value * 1000),
                "remainingDays": exp.map(|value| ((value - now) as f64 / 86400.0).floor() as i64),
                "hasRefresh": token.refresh.is_some(),
            })
        }
        None => json!({ "loggedIn": false }),
    }
}

pub fn logout() -> Value {
    let _ = fs::remove_file(token_file());
    log_line("已退出登录");
    json!({ "loggedIn": false })
}

// ---------------------------------------------------------------------------
// 托盘调度：本地 OpenAI 兼容服务
// ---------------------------------------------------------------------------

static SERVER: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
static STARTING: Mutex<bool> = Mutex::new(false);

fn running() -> bool {
    let mut guard = SERVER.lock().unwrap_or_else(|p| p.into_inner());
    match guard.as_ref() {
        Some(handle) => {
            if handle.is_finished() {
                *guard = None;
                false
            } else {
                true
            }
        }
        None => false,
    }
}

pub async fn start() -> Result<Value, String> {
    {
        let mut starting = STARTING.lock().unwrap_or_else(|p| p.into_inner());
        if *starting {
            return Err("正在启动中".into());
        }
        *starting = true;
    }
    let result = start_inner().await;
    *STARTING.lock().unwrap_or_else(|p| p.into_inner()) = false;
    result
}

async fn start_inner() -> Result<Value, String> {
    if running() {
        return Ok(status().await);
    }
    if read_token().is_none() {
        return Err("还没有登录国际版账号，请先点「登录国际版」".into());
    }
    if std::net::TcpStream::connect(("127.0.0.1", PORT)).is_ok() {
        return Err(format!(
            "端口 {PORT} 已被占用。若之前用独立脚本跑过摆渡，请先停掉它再启动。"
        ));
    }
    ensure_dir()?;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", PORT))
        .await
        .map_err(|error| format!("绑定端口 {PORT} 失败：{error}"))?;

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let service = service_fn(route);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    *SERVER.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
    let _ = fs::write(enabled_marker(), b"1");
    log_line(&format!("服务已启动 http://127.0.0.1:{PORT}/v1"));
    Ok(status().await)
}

pub async fn stop() -> Result<Value, String> {
    let handle = SERVER.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(handle) = handle {
        handle.abort();
        // abort 到端口真正释放有一小段延迟，等它一下，避免「刚停就起」撞端口。
        for _ in 0..50 {
            if std::net::TcpStream::connect(("127.0.0.1", PORT)).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let _ = fs::remove_file(enabled_marker());
    log_line("服务已停止");
    Ok(json!({ "running": false, "phase": "stopped", "message": "已停止" }))
}

/// 退出钩子用的同步版本：事件循环退出时没有 tokio 运行时可以 await。
pub fn stop_blocking() {
    if let Some(handle) = SERVER.lock().unwrap_or_else(|p| p.into_inner()).take() {
        handle.abort();
    }
}

pub async fn status() -> Value {
    let token = read_token();
    let is_running = running();
    let mut base = json!({
        "running": is_running,
        "port": PORT,
        "endpoint": format!("http://127.0.0.1:{PORT}/v1"),
        "dataDir": data_dir().to_string_lossy(),
        "loggedIn": token.is_some(),
        "account": token.as_ref().and_then(|t| t.account.clone()),
    });
    // 头部一行要显示「共 N 个 · M 个免费 · K 个已选」，目录有 30 秒缓存，成本可忽略。
    let catalog = catalog_cached();
    base["models"] = json!({
        "total": catalog.models.len(),
        "free": catalog.models.iter().filter(|model| model.free).count(),
        "selected": effective_selection(&catalog.models).len(),
    });
    if !is_running {
        base["phase"] = json!("stopped");
        base["message"] = json!(if token.is_some() { "未在运行" } else { "未登录" });
        return base;
    }
    base["phase"] = json!("ready");
    base["message"] = json!("运行中");
    base
}

/// 应用启动时的自动拉起：只有用户上次主动开过才启动。
pub fn autostart_on_launch() {
    if !enabled_marker().exists() {
        return;
    }
    tauri::async_runtime::spawn(async {
        if let Err(error) = start().await {
            log_line(&format!("[autostart] {error}"));
        }
    });
}

/// 日志尾部：一行一条，前端只读不写。
pub fn logs(lines: usize) -> Value {
    let text = fs::read_to_string(log_file()).unwrap_or_default();
    let tail: Vec<String> = text
        .lines()
        .rev()
        .take(lines)
        .map(str::to_string)
        .collect();
    json!({
        "lines": tail.into_iter().rev().collect::<Vec<String>>(),
        "file": log_file().to_string_lossy(),
    })
}

// ---------------------------------------------------------------------------
// 请求体归一化与 SSE 聚合
// ---------------------------------------------------------------------------

/// 上游只收流式、且首条必须是 system。这里统一补齐，客户端不需要知道这些约束。
fn normalize(mut body: Value) -> Value {
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        let first_is_system = messages
            .first()
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("system");
        if !first_is_system {
            messages.insert(0, json!({ "role": "system", "content": DEFAULT_SYSTEM }));
        }
    } else {
        body["messages"] = json!([
            { "role": "system", "content": DEFAULT_SYSTEM }
        ]);
    }
    body["stream"] = json!(true);
    if let Some(object) = body.as_object_mut() {
        // stream_options 是 OpenAI 的用法，上游不认，去掉免得判非法请求。
        object.remove("stream_options");
    }
    body
}

#[derive(Default)]
struct Agg {
    id: Option<String>,
    model: Option<String>,
    created: Option<i64>,
    text: String,
    reason: String,
    usage: Option<Value>,
    finish: Option<String>,
    role: Option<String>,
    /// index → (id, name, arguments)
    tools: HashMap<u32, (String, String, String)>,
}

impl Agg {
    fn feed_line(&mut self, line: &str) {
        let line = line.trim();
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        if self.id.is_none() {
            self.id = value.get("id").and_then(Value::as_str).map(str::to_string);
        }
        if self.model.is_none() {
            self.model = value.get("model").and_then(Value::as_str).map(str::to_string);
        }
        if let Some(created) = value.get("created").and_then(Value::as_i64) {
            self.created = Some(created);
        }
        if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
            self.usage = Some(usage.clone());
        }
        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if !reason.is_empty() {
                    self.finish = Some(reason.to_string());
                }
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(role) = delta.get("role").and_then(Value::as_str) {
                self.role = Some(role.to_string());
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                self.text.push_str(text);
            }
            if let Some(reason) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
            {
                self.reason.push_str(reason);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let slot = self.tools.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        slot.0 = id.to_string();
                    }
                    if let Some(function) = call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            slot.1.push_str(name);
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                            slot.2.push_str(arguments);
                        }
                    }
                }
            }
        }
    }

    fn into_completion(self) -> Value {
        let has_tools = !self.tools.is_empty();
        let mut message = json!({
            "role": self.role.unwrap_or_else(|| "assistant".into()),
            "content": if self.text.is_empty() && has_tools { Value::Null } else { json!(self.text) },
        });
        if !self.reason.is_empty() {
            message["reasoning_content"] = json!(self.reason);
        }
        if has_tools {
            let mut indexes: Vec<&u32> = self.tools.keys().collect();
            indexes.sort();
            let calls: Vec<Value> = indexes
                .into_iter()
                .map(|index| {
                    let (id, name, arguments) = &self.tools[index];
                    json!({
                        "id": if id.is_empty() { format!("call_{index}") } else { id.clone() },
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": if arguments.is_empty() { "{}".into() } else { arguments.clone() },
                        },
                    })
                })
                .collect();
            message["tool_calls"] = json!(calls);
        }
        let finish = self.finish.unwrap_or_else(|| {
            if has_tools {
                "tool_calls".into()
            } else {
                "stop".into()
            }
        });
        let mut out = json!({
            "id": self.id.unwrap_or_else(|| format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())),
            "object": "chat.completion",
            "created": self.created.unwrap_or_else(|| paths::now_ms() / 1000),
            "model": self.model.unwrap_or_else(|| "deepseek-v4.1-flash".into()),
            "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        });
        if let Some(usage) = self.usage {
            out["usage"] = usage;
        }
        out
    }
}

/// 把上游 SSE 读干净并聚合成一条 `chat.completion`。
async fn aggregate(mut response: reqwest::Response) -> Result<Value, String> {
    let mut agg = Agg::default();
    let mut buffer = String::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("读取上游数据失败：{error}"))?
    {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(position) = buffer.find('\n') {
            let line: String = buffer.drain(..=position).collect();
            agg.feed_line(line.trim_end_matches(['\r', '\n']));
        }
    }
    if !buffer.trim().is_empty() {
        agg.feed_line(buffer.trim_end_matches(['\r', '\n']));
    }
    Ok(agg.into_completion())
}

// ---------------------------------------------------------------------------
// HTTP 服务
// ---------------------------------------------------------------------------

type Body = BoxBody<Bytes, std::io::Error>;

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let bytes = Bytes::from(serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()));
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Access-Control-Allow-Origin", "*")
        .body(
            Full::new(bytes)
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("构造 JSON 响应失败")
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response<Body> {
    json_response(status, json!({ "error": { "message": message.into() } }))
}

async fn route(request: Request<Incoming>) -> Result<Response<Body>, std::convert::Infallible> {
    let path = request.uri().path().trim_end_matches('/').to_string();
    let method = request.method().clone();

    let response = match (method, path.as_str()) {
        (Method::OPTIONS, _) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("Access-Control-Allow-Origin", "*")
            .header("Access-Control-Allow-Headers", "*")
            .header("Access-Control-Allow-Methods", "GET,POST,OPTIONS")
            .body(Full::new(Bytes::new()).map_err(|never| match never {}).boxed())
            .expect("构造预检响应失败"),
        (Method::GET, p) if p.ends_with("/models") => {
            let catalog = catalog_cached();
            json_response(
                StatusCode::OK,
                json!({
                    "object": "list",
                    "data": catalog.models.iter().map(|model| json!({
                        "id": client_model_id(model),
                        "object": "model",
                        "owned_by": "workbuddy-ai",
                        "name": model.name,
                        "credits": model.credits,
                    })).collect::<Vec<Value>>(),
                }),
            )
        }
        (Method::GET, p) if p.ends_with("/health") => json_response(
            StatusCode::OK,
            json!({ "ok": true, "upstream": format!("{API}/v2/chat/completions"), "channel": "direct" }),
        ),
        (Method::POST, p) if p.ends_with("/chat/completions") => {
            chat(request).await
        }
        _ => error_response(StatusCode::NOT_FOUND, "未知路径"),
    };
    Ok(response)
}

async fn chat(request: Request<Incoming>) -> Response<Body> {
    let body_bytes = match request.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => return error_response(StatusCode::BAD_REQUEST, format!("读取请求体失败：{error}")),
    };
    let body: Value = match serde_json::from_slice(&body_bytes) {
        Ok(value) => value,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, format!("请求体不是 JSON：{error}")),
    };

    let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut payload = normalize(body);
    // 客户端发过来的是我们导入时加的 `wbai/xxx`，上游只认裸 id，这里剥掉。
    let requested = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let model = strip_client_prefix(&requested).to_string();
    payload["model"] = json!(model);
    if model.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "请求里没有 model");
    }

    let Some(token) = read_token() else {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "还没有登录国际版账号");
    };

    // 401/403 时刷新一次再重试；连不上则重试两次（上游偶发 TLS 抖动）。
    let mut access = token.access;
    let mut response = None;
    for attempt in 0..3 {
        let result = client()
            .post(format!("{API}/v2/chat/completions"))
            .headers(upstream_headers(&access))
            .json(&payload)
            .send()
            .await;
        match result {
            Ok(value) => {
                let status = value.status();
                if matches!(status.as_u16(), 401 | 403) && attempt == 0 {
                    match refresh_token().await {
                        Ok(fresh) => {
                            access = fresh;
                            continue;
                        }
                        Err(error) => {
                            log_line(&format!("刷新失败：{error}"));
                        }
                    }
                }
                response = Some(value);
                break;
            }
            Err(error) => {
                log_line(&format!("建连失败 第{}次：{error}", attempt + 1));
                if attempt == 2 {
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        format!("上游不可达：{error}"),
                    );
                }
                tokio::time::sleep(Duration::from_millis(400 * (1 << attempt))).await;
            }
        }
    }

    let Some(response) = response else {
        return error_response(StatusCode::BAD_GATEWAY, "上游没有返回");
    };

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        log_line(&format!(
            "上游 {status} model={model}：{}",
            text.chars().take(300).collect::<String>()
        ));
        // 上游的错误体本身就是给客户端看的，能原样透就原样透。
        return match serde_json::from_str::<Value>(&text) {
            Ok(value) => json_response(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                value,
            ),
            Err(_) => json_response(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                json!({ "error": { "message": text.chars().take(600).collect::<String>() } }),
            ),
        };
    }

    if !want_stream {
        return match aggregate(response).await {
            Ok(completion) => {
                let finish = completion["choices"][0]["finish_reason"]
                    .as_str()
                    .unwrap_or("stop");
                log_line(&format!("完成 model={model} finish={finish} 非流式"));
                json_response(StatusCode::OK, completion)
            }
            Err(error) => error_response(StatusCode::BAD_GATEWAY, error),
        };
    }

    log_line(&format!("开始流式 model={model}"));
    let stream = response
        .bytes_stream()
        .map_err(|error| std::io::Error::other(error))
        .map_ok(Frame::data);
    let body = BodyExt::boxed(StreamBody::new(stream));
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream; charset=utf-8")
        .header("Cache-Control", "no-cache")
        .header("Access-Control-Allow-Origin", "*")
        .body(body)
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "构造流式响应失败"))
}

// ---------------------------------------------------------------------------
// 面板数据：目录 + 选择 + 探测缓存
// ---------------------------------------------------------------------------

/// 给面板一整份数据。面板只在切到本页和操作后拉一次，不走状态轮询 ——
/// 目录有 30 秒缓存，探测结论有 10 分钟缓存，重复拉也不贵。
pub fn models_view() -> Value {
    let catalog = catalog_cached();
    let selected = effective_selection(&catalog.models);
    let chosen: std::collections::HashSet<&str> = selected.iter().map(String::as_str).collect();
    let probes = read_probe_cache();

    let items: Vec<Value> = catalog
        .models
        .iter()
        .map(|model| {
            json!({
                "id": model.id,
                "name": model.name,
                "credits": model.credits,
                "free": model.free,
                "input": model.input,
                "output": model.output,
                "images": model.images,
                "tools": model.tools,
                "reasoning": model.reasoning,
                "selected": chosen.contains(model.id.as_str()),
                "verdict": probes.get(&model.id).cloned().unwrap_or(Value::Null),
            })
        })
        .collect();

    let free = catalog.models.iter().filter(|m| m.free).count();
    let ready = probes
        .values()
        .filter(|value| value.get("ok").and_then(Value::as_bool) == Some(true))
        .count();

    json!({
        "models": items,
        "source": catalog.source,
        "total": catalog.models.len(),
        "free": free,
        "selected": selected.len(),
        "ready": ready,
        "catalogDir": catalog_dir().to_string_lossy(),
        "endpoint": format!("http://127.0.0.1:{PORT}/v1"),
    })
}

/// 勾选/取消一个模型。面板的开关只落到 `selection.json`，不碰 models.json ——
/// 用户可以先挑好，再一次性导入。
pub fn set_selection(model: String, selected: bool) -> Result<Value, String> {
    let catalog = catalog_cached();
    if !catalog.models.iter().any(|entry| entry.id == model) {
        return Err(format!("目录里没有这个模型：{model}"));
    }
    let mut list = effective_selection(&catalog.models);
    let position = list.iter().position(|id| id == &model);
    match (selected, position) {
        (true, None) => list.push(model.clone()),
        (false, Some(index)) => {
            list.remove(index);
        }
        _ => {}
    }
    write_selection(&list)?;
    Ok(json!({ "ok": true, "model": model, "selected": selected, "count": list.len() }))
}

// ---------------------------------------------------------------------------
// 导入到国内版模型列表
// ---------------------------------------------------------------------------

fn models_file() -> PathBuf {
    paths::home_dir().join(".workbuddy/models.json")
}

/// 这条记录是不是我们放的。
///
/// 除了认 `wbaiOwner` 字段，还认指向本机转发口的 url —— 上一版导入的条目没有
/// 那个字段，靠 url 才能一起清掉，否则升级后会留下一条旧 id 的重复模型。
fn is_owned_entry(item: &Value) -> bool {
    if item.get(OWNER_FIELD).and_then(Value::as_str) == Some(OWNER) {
        return true;
    }
    item.get("url")
        .and_then(Value::as_str)
        .map(|url| {
            url.contains(&format!("127.0.0.1:{PORT}")) || url.contains(&format!("localhost:{PORT}"))
        })
        .unwrap_or(false)
}

fn client_model_id(model: &ModelEntry) -> String {
    format!("{CLIENT_PREFIX}{}", model.id)
}

/// 客户端可见 id（`wbai/xxx`）还原成上游真实 id。用户在 WorkBuddy 里看到的就是
/// 我们加的 id，原样发上去会被上游判成「不认识这个模型」。
fn strip_client_prefix(model: &str) -> &str {
    model.strip_prefix(CLIENT_PREFIX).unwrap_or(model)
}

/// 备份 models.json。只留最近 5 份：重启频繁时别把用户目录塞满。
fn backup_models(path: &PathBuf) {
    let Some(dir) = path.parent() else {
        return;
    };
    let stamp = paths::now_ms();
    let target = dir.join(format!("models.json.wbai-{stamp}.bak"));
    if fs::copy(path, &target).is_err() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("models.json.wbai-") && name.ends_with(".bak"))
                .unwrap_or(false)
        })
        .collect();
    backups.sort();
    let excess = backups.len().saturating_sub(5);
    for path in backups.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}

/// 原子替换：先写临时文件再 rename，中途失败不会留下半份 models.json。
fn replace_file(path: &PathBuf, text: &str) -> std::io::Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    fs::write(&temp, text)?;
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// 把勾选的模型写进国内版 `~/.workbuddy/models.json`。
///
/// 规则与「模型摆渡」一致：只增删自己标记过的条目，用户手写的模型一条都不改；
/// id 撞上了就跳过（宁可少导一个，也不能覆盖别人的配置）。
pub fn import_selected() -> Value {
    import_into(&models_file())
}

/// 真正干活的版本，路径可注入 —— 单元测试要在临时目录里验证合并规则，
/// 不能拿用户真实的 models.json 做实验。
fn import_into(path: &PathBuf) -> Value {
    let catalog = catalog_cached();
    let selected = effective_selection(&catalog.models);
    if selected.is_empty() {
        return json!({ "ok": false, "error": "没勾选任何模型。先在列表里打开要导入的模型。" });
    }
    let chosen: std::collections::HashSet<&str> = selected.iter().map(String::as_str).collect();
    let endpoint = format!("http://127.0.0.1:{PORT}/v1/chat/completions");

    // 读-改-写之间可能被别的工具（模型摆渡、用户手改）动过，被动了就重来一遍，
    // 最多三轮；三轮还撞就如实报错，绝不硬写覆盖。
    for attempt in 0..3 {
        let original = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "[]".to_string(),
            Err(error) => {
                return json!({ "ok": false, "error": format!("读 models.json 失败：{error}") })
            }
        };
        let document: Value = match serde_json::from_str(&original) {
            Ok(value) => value,
            Err(error) => {
                return json!({
                    "ok": false,
                    "error": format!("models.json 不是合法 JSON，没有改动：{error}"),
                })
            }
        };

        // 兼容两种形态：数组，或 `{ models: [...] }`。
        let (list, mut wrapper) = match document {
            Value::Array(items) => (items, None),
            Value::Object(object) => {
                let items = object
                    .get("models")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                (items, Some(object))
            }
            _ => {
                return json!({ "ok": false, "error": "models.json 结构不认识，没有改动" });
            }
        };

        let removed: Vec<String> = list
            .iter()
            .filter(|item| is_owned_entry(item))
            .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let kept: Vec<Value> = list
            .into_iter()
            .filter(|item| !is_owned_entry(item))
            .collect();
        let mut taken: std::collections::HashSet<String> = kept
            .iter()
            .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();

        let mut entries: Vec<Value> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for model in catalog.models.iter().filter(|m| chosen.contains(m.id.as_str())) {
            let id = client_model_id(model);
            if taken.contains(&id) || taken.contains(&model.id) {
                skipped.push(model.id.clone());
                continue;
            }
            taken.insert(id.clone());
            entries.push(json!({
                "id": id,
                // 名字里带「国际」是为了在 WorkBuddy 的下拉里一眼看出走的是哪条链路。
                "name": format!("国际 · {}", model.name),
                "vendor": "Custom",
                "url": endpoint,
                "apiKey": "local",
                "maxInputTokens": if model.input > 0 { json!(model.input) } else { Value::Null },
                "maxOutputTokens": if model.output > 0 { json!(model.output) } else { Value::Null },
                "supportsToolCall": model.tools,
                "supportsImages": model.images,
                "supportsReasoning": model.reasoning,
                "useCustomProtocol": false,
                OWNER_FIELD: OWNER,
            }));
        }

        let count = entries.len();
        let mut combined = kept.clone();
        combined.extend(entries);

        let text = match wrapper.as_mut() {
            Some(object) => {
                object.insert("models".into(), json!(combined));
                // 对象形态里可能还有一份可用 id 列表：把自己刚摘掉的 id 一起摘掉，
                // 用户自己加进这份列表的 id 保持原样。
                if let Some(available) = object.get_mut("availableModels").and_then(Value::as_array_mut) {
                    available.retain(|id| {
                        id.as_str()
                            .map(|id| !removed.iter().any(|gone| gone == id))
                            .unwrap_or(true)
                    });
                }
                serde_json::to_string_pretty(&Value::Object(object.clone()))
            }
            None => serde_json::to_string_pretty(&Value::Array(combined)),
        };
        let Ok(text) = text else {
            return json!({ "ok": false, "error": "序列化 models.json 失败" });
        };

        // rename 之前再确认一次：原文没被别人改过才落盘。
        match fs::read_to_string(path) {
            Ok(current) if current != original => {
                if attempt == 2 {
                    return json!({ "ok": false, "error": "models.json 正在被其它程序修改，稍后重试" });
                }
                continue;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return json!({ "ok": false, "error": format!("复核 models.json 失败：{error}") })
            }
        }

        if path.exists() {
            backup_models(path);
        }
        if let Err(error) = replace_file(path, &text) {
            return json!({ "ok": false, "error": format!("写 models.json 失败：{error}") });
        }
        log_line(&format!("已导入 {count} 个模型到 {}", path.to_string_lossy()));
        return json!({
            "ok": true,
            "count": count,
            "skipped": skipped,
            "file": path.to_string_lossy(),
            "endpoint": format!("http://127.0.0.1:{PORT}/v1"),
            "restart": "需要完全退出并重开国内版 WorkBuddy 才能在模型列表里看到",
        });
    }

    json!({ "ok": false, "error": "写 models.json 失败：重试三轮仍未拿到稳定的文件" })
}

/// `availableModels` 里上一次留下的自有 id 要被摘掉，判断依据就是刚才被清掉的
/// 自有条目 id。用户自己加进这份列表的 id 原样留着。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_inserts_system() {
        let body = json!({ "model": "x", "messages": [{ "role": "user", "content": "hi" }] });
        let out = normalize(body);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["stream"], true);
    }

    #[test]
    fn normalize_keeps_existing_system() {
        let body = json!({
            "messages": [
                { "role": "system", "content": "a" },
                { "role": "user", "content": "b" }
            ]
        });
        let out = normalize(body);
        assert_eq!(out["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn aggregate_collects_text_and_tools() {
        let mut agg = Agg::default();
        agg.feed_line(r#"data: {"id":"x","model":"m","choices":[{"index":0,"delta":{"content":"你"}}]}"#);
        agg.feed_line(r#"data: {"id":"x","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"f","arguments":"{}"}}]}}]}"#);
        let out = agg.into_completion();
        assert_eq!(out["choices"][0]["message"]["content"], "你");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn credits_label_reads_all_known_shapes() {
        assert_eq!(credits_label(&json!({ "credits": "x0.00" })), ("免费".into(), true));
        assert_eq!(
            credits_label(&json!({ "credits": "x0.29 credits" })),
            ("0.29×".into(), false)
        );
        assert_eq!(
            credits_label(&json!({ "credits": "x10.00 credits" })),
            ("10.00×".into(), false)
        );
        let (label, free) = credits_label(&json!({}));
        assert_eq!(label, "—");
        assert!(!free);
    }

    #[test]
    fn client_prefix_is_reversible() {
        for model in catalog().models {
            let client = client_model_id(&model);
            assert_eq!(strip_client_prefix(&client), model.id);
        }
        // 用户手写的裸 id 不能被误伤。
        assert_eq!(strip_client_prefix("hy3"), "hy3");
    }

    #[test]
    fn owned_entries_are_recognized_by_field_or_url() {
        assert!(is_owned_entry(
            &json!({ "url": format!("http://127.0.0.1:{PORT}/v1/chat/completions") })
        ));
        assert!(is_owned_entry(&json!({ OWNER_FIELD: OWNER })));
        assert!(!is_owned_entry(
            &json!({ "url": "https://api.deepseek.com/chat/completions" })
        ));
    }

    #[test]
    fn probe_errors_are_classified() {
        // 11158 是「账号类型不支持」，实测 personal_free_user 调 gpt-5.6-luna 就是它。
        assert_eq!(
            classify_probe_error(Some(11158), "model is not allowed for user type").0,
            "not_allowed"
        );
        assert_eq!(
            classify_probe_error(None, "This model is not available in your country").0,
            "error"
        );
        assert_eq!(
            classify_probe_error(None, "model [x] does not exist").0,
            "not_found"
        );
        assert_eq!(classify_probe_error(Some(0), "").0, "error");
    }

    #[test]
    fn catalog_never_empty() {
        let catalog = catalog();
        assert!(!catalog.models.is_empty(), "目录必须有兜底清单");
        // 没有上下文长度的图片/视频模型不该被列成对话模型。
        assert!(catalog.models.iter().all(|model| model.input > 0));
    }

    /// 导入规则的回归：只动自己的条目、用户手写的一条不少、重复导入不累加。
    /// 全程在临时目录里跑，不碰真实的 `~/.workbuddy/models.json`。
    #[test]
    fn import_keeps_user_entries_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("wbai-test-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        let user_entry = json!({
            "id": "hy3",
            "name": "我自己配的 hy3",
            "url": "https://example.com/v1/chat/completions",
            "apiKey": "sk-test",
        });
        fs::write(&path, serde_json::to_string_pretty(&json!([user_entry.clone()])).unwrap()).unwrap();

        let first = import_into(&path);
        assert_eq!(first["ok"], true, "第一次导入应成功：{first}");
        let items: Vec<Value> = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            items.iter().any(|item| item["id"] == "hy3" && item["url"] == "https://example.com/v1/chat/completions"),
            "用户自己的条目被改动了"
        );
        let ours: Vec<&Value> = items
            .iter()
            .filter(|item| item.get(OWNER_FIELD).and_then(Value::as_str) == Some(OWNER))
            .collect();
        assert!(!ours.is_empty(), "没有写入任何自有条目");
        assert!(ours
            .iter()
            .all(|item| item["id"].as_str().unwrap_or("").starts_with(CLIENT_PREFIX)));
        assert!(
            !ours.iter().any(|item| item["id"] == "hy3"),
            "和用户条目撞 id 时应当跳过而不是覆盖"
        );

        let before = items.len();
        let second = import_into(&path);
        assert_eq!(second["ok"], true, "第二次导入应成功：{second}");
        let again: Vec<Value> = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(again.len(), before, "重复导入不该累加条目");
        assert!(again.iter().any(|item| item["id"] == "hy3" && item["url"] == "https://example.com/v1/chat/completions"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// 真跑一次「写入真实 models.json」的端到端验证：会留下备份，条目带自己的标记，
    /// 下次从面板导入会整体替换掉，不会累积。手动执行：
    /// `cargo test --lib import_selected_real -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "会写真实 models.json，手动执行"]
    async fn import_selected_real() {
        let result = import_selected();
        println!("{result}");
        assert_eq!(result["ok"], true, "导入失败：{result}");
        let items: Vec<Value> =
            serde_json::from_str(&fs::read_to_string(models_file()).unwrap()).unwrap();
        let ours = items
            .iter()
            .filter(|item| item.get(OWNER_FIELD).and_then(Value::as_str) == Some(OWNER))
            .count();
        assert_eq!(ours, result["count"].as_u64().unwrap_or(0) as usize);
    }

    /// 联网冒烟：验证「在应用里能发起国际版登录」这条链路本身是通的。
    /// 只申请 state 与 authUrl，不代替用户完成登录，跑完不留任何副作用。
    /// 手动执行：`cargo test wbai_login -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "联网测试，手动执行"]
    async fn login_start_returns_official_url() {
        let started = start_login().await.expect("申请登录状态失败");
        let url = started["authUrl"].as_str().unwrap_or_default();
        assert!(url.starts_with("https://www.workbuddy.ai/"), "登录地址不在官方域名内：{url}");
        let login_id = started["loginId"].as_str().expect("没有 loginId");
        assert!(cancel_login(login_id));
    }
}
