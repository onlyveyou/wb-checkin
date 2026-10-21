//! 数据目录、私有文件写入与配置。
//!
//! 安全基线（对照同类工具常见缺陷）：
//! - 数据目录 ~/.wb-checkin 固定 0700，凭证文件固定 0600；
//!   临时文件在创建时即带 0600，避免 rename 之前出现可被同机其他用户读取的窗口。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn home_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
}

pub fn data_dir() -> PathBuf {
    home_dir().join(".wb-checkin")
}

/// 展开开头的 `~`。只处理 `~` 与 `~/`，`~user` 原样返回——宁可不展开，
/// 也不要把别人的家目录猜错。
pub fn expand_tilde(input: &str) -> PathBuf {
    let trimmed = input.trim();
    if trimmed == "~" {
        return home_dir();
    }
    match trimmed.strip_prefix("~/") {
        Some(rest) => home_dir().join(rest),
        None => PathBuf::from(trimmed),
    }
}

pub fn accounts_file() -> PathBuf {
    data_dir().join("accounts.json")
}

pub fn config_file() -> PathBuf {
    data_dir().join("config.json")
}

pub fn logs_file() -> PathBuf {
    data_dir().join("checkin-logs.json")
}

/// 建目录并收紧到 0700。
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir)?;
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// 原子写 + 0600。
pub fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(parent)?;

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "data".to_string());
    let tmp = path.with_file_name(format!("{name}.tmp-{}", uuid::Uuid::new_v4().simple()));

    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
    }

    fs::rename(&tmp, path)?;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    Ok(())
}

pub fn read_json_array(path: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

pub fn read_json_object(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.is_object().then_some(value)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 本地 AI 监控配置
//
// 这块是可选的：只有部署了本地推理后端（llama.cpp / 网关）的机器才需要打开。
// 因此默认关闭，且所有路径字段默认留空——空值在运行期解析成「约定位置」，
// 这样同一份配置在部署机与未部署机上都不会写出错误路径。
// ---------------------------------------------------------------------------

/// 一个被监控的本地服务。默认值与参考实现（ai_status_dashboard.py）一致。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LocalAiService {
    pub key: String,
    pub port: u16,
    pub name: String,
    #[serde(default)]
    pub desc: String,
    /// 只做 TCP 探测、绝不发 HTTP。
    ///
    /// 按需网关空闲时会自动关停，任何 HTTP 请求都会把它唤醒并触发 9~29 秒的冷启动。
    /// 探活本身不该有副作用，所以网关这类服务只验端口是否在听。
    #[serde(default)]
    pub tcp_only: bool,
}

/// 本地 AI 监控的开关与路径。全部路径字段支持 `~` 前缀。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LocalAiConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 日志目录。留空表示使用约定位置 `~/llama.cpp/logs`。
    #[serde(default)]
    pub log_dir: String,
    /// smctemp 可执行文件路径。留空表示按约定位置自动查找。
    #[serde(default)]
    pub smctemp_path: String,
    #[serde(default = "default_local_ai_services")]
    pub services: Vec<LocalAiService>,
}

/// 约定日志目录：`~/llama.cpp/logs`。
pub fn default_log_dir() -> PathBuf {
    home_dir().join("llama.cpp").join("logs")
}

/// smctemp 的候选位置，按优先级排列。前面的命中就不再往后找。
pub fn smctemp_candidates(configured: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !configured.trim().is_empty() {
        out.push(expand_tilde(configured));
    }
    out.push(data_dir().join("bin").join("smctemp"));
    out.push(home_dir().join("llama.cpp").join("llm-serve").join("smctemp"));
    out.push(PathBuf::from("/opt/homebrew/bin/smctemp"));
    out.push(PathBuf::from("/usr/local/bin/smctemp"));
    out.push(home_dir().join("bin").join("smctemp"));
    out
}

fn default_local_ai_services() -> Vec<LocalAiService> {
    vec![
        LocalAiService {
            key: "gateway".to_string(),
            port: 8080,
            name: "按需网关 (统一入口)".to_string(),
            desc: "空闲 300s 自动关停，请求时自动拉起（冷启动 9-29s）".to_string(),
            tcp_only: true,
        },
        LocalAiService {
            key: "text".to_string(),
            port: 8081,
            name: "文本后端".to_string(),
            desc: "分级/翻译主力".to_string(),
            tcp_only: false,
        },
        LocalAiService {
            key: "vision".to_string(),
            port: 18080,
            name: "视觉后端".to_string(),
            desc: "验证码 / 画面理解".to_string(),
            tcp_only: false,
        },
    ]
}

impl LocalAiConfig {
    /// 收敛非法值：端口为 0 的服务没有意义，直接剔掉；服务列表为空时回落到默认。
    pub fn normalized(&self) -> Self {
        let services: Vec<LocalAiService> = self
            .services
            .iter()
            .filter(|service| service.port > 0)
            .map(|service| LocalAiService {
                key: service.key.trim().to_string(),
                port: service.port,
                name: service.name.trim().to_string(),
                desc: service.desc.trim().to_string(),
                tcp_only: service.tcp_only,
            })
            .collect();

        Self {
            enabled: self.enabled,
            log_dir: self.log_dir.trim().to_string(),
            smctemp_path: self.smctemp_path.trim().to_string(),
            services: if services.is_empty() {
                default_local_ai_services()
            } else {
                services
            },
        }
    }
}

impl Default for LocalAiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            log_dir: String::new(),
            smctemp_path: String::new(),
            services: default_local_ai_services(),
        }
    }
}

fn default_auto_enabled() -> bool {
    true
}

fn default_backup_hour() -> u32 {
    12
}

fn default_backup_minute() -> u32 {
    0
}

/// 菜单栏默认只显示网速与硬盘 —— 用户最初要的就是这两项，CPU 与内存
/// 需要自己去设置里勾，免得四个数字默认就把菜单栏撑满。
fn default_menubar_items() -> Vec<String> {
    vec!["net".to_string(), "disk".to_string()]
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppConfig {
    /// 自动签到总开关。
    #[serde(default = "default_auto_enabled")]
    pub auto_enabled: bool,
    /// 每日兜底签到的时刻（时）。
    ///
    /// 触发策略是「启动即签 + 每日兜底」：App 启动后立即签到一次，此后每天到达
    /// 该时刻再签一次。签到接口幂等，当天已签到时重复调用只会返回「已签到」，
    /// 因此两次触发不会造成重复提交。
    #[serde(default = "default_backup_hour")]
    pub backup_hour: u32,
    /// 每日兜底签到的时刻（分）。
    #[serde(default = "default_backup_minute")]
    pub backup_minute: u32,
    /// 本地 AI 监控（可选功能，默认关闭）。
    #[serde(default)]
    pub local_ai: LocalAiConfig,
    /// 菜单栏要显示哪些指标：`net` / `disk` / `cpu` / `mem`。
    ///
    /// 空数组表示只留图标、不显示文字，与「一个都不勾」等价。
    #[serde(default = "default_menubar_items")]
    pub menubar_items: Vec<String>,
}

impl AppConfig {
    /// 兜底时刻收敛到合法区间，避免配置文件被手改成 99:99。
    pub fn normalized(&self) -> Self {
        Self {
            auto_enabled: self.auto_enabled,
            backup_hour: self.backup_hour.min(23),
            backup_minute: self.backup_minute.min(59),
            local_ai: self.local_ai.normalized(),
            // 走 sysmon 的白名单，顺手剔掉手改配置写进来的乱七八糟标识。
            menubar_items: crate::sysmon::normalize(&self.menubar_items),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            auto_enabled: default_auto_enabled(),
            backup_hour: default_backup_hour(),
            backup_minute: default_backup_minute(),
            local_ai: LocalAiConfig::default(),
            menubar_items: default_menubar_items(),
        }
    }
}

/// 纯计算：`now` 是否已到达当天的 `hour:minute`（含正好等于）。
///
/// 用墙钟比较而非「睡到目标时刻」的定时器，因为系统睡眠期间单调时钟不走，
/// 定时器会在唤醒后继续倒数，使兜底签到被推迟到第二天。
fn reached(now: chrono::NaiveDateTime, hour: u32, minute: u32) -> bool {
    let target = now
        .date()
        .and_hms_opt(hour.min(23), minute.min(59), 0)
        .unwrap_or(now);
    now >= target
}

/// 今天是否已到达兜底时刻。
pub fn today_reached(hour: u32, minute: u32) -> bool {
    reached(chrono::Local::now().naive_local(), hour, minute)
}

pub fn load_config() -> AppConfig {
    read_json_object(&config_file())
        .and_then(|v| serde_json::from_value::<AppConfig>(v).ok())
        .unwrap_or_default()
        .normalized()
}

pub fn save_config(config: &AppConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    write_private(&config_file(), &text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_private_keeps_dir_0700_and_file_0600() {
        let base = std::env::temp_dir().join(format!("wb-checkin-test-{}", uuid::Uuid::new_v4().simple()));
        let file = base.join("accounts.json");

        write_private(&file, "[]").expect("首次写入应成功");
        assert_eq!(mode_of(&base), 0o700, "数据目录必须是 0700");
        assert_eq!(mode_of(&file), 0o600, "账号库必须是 0600");

        // 覆盖写之后权限不能被放宽
        write_private(&file, "[1]").expect("覆盖写应成功");
        assert_eq!(mode_of(&file), 0o600, "覆盖写后仍须是 0600");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn config_round_trips_with_expected_defaults() {
        let config = AppConfig::default();
        assert!(config.auto_enabled, "自动签到默认开启");
        assert_eq!((config.backup_hour, config.backup_minute), (12, 0));

        let parsed: AppConfig = serde_json::from_str("{}").expect("缺字段应回落到默认值");
        assert!(parsed.auto_enabled);
        assert_eq!((parsed.backup_hour, parsed.backup_minute), (12, 0));
    }

    #[test]
    fn legacy_config_with_interval_still_loads() {
        // 旧版本写下的 config.json 含 interval_minutes，升级后不能让整体解析失败。
        let legacy = r#"{"auto_enabled": false, "interval_minutes": 30}"#;
        let parsed: AppConfig = serde_json::from_str(legacy).expect("旧配置应仍可解析");
        assert!(!parsed.auto_enabled, "旧配置里的开关要保留");
        assert_eq!((parsed.backup_hour, parsed.backup_minute), (12, 0), "兜底时刻回落到默认");
    }

    #[test]
    fn backup_time_is_clamped_to_valid_range() {
        let parsed: AppConfig =
            serde_json::from_str(r#"{"backup_hour": 99, "backup_minute": 99}"#).expect("应可解析");
        let fixed = parsed.normalized();
        assert_eq!((fixed.backup_hour, fixed.backup_minute), (23, 59));
    }

    #[test]
    fn backup_moment_not_reached_before_target() {
        let now = at(2026, 9, 16, 9, 0);
        assert!(!reached(now, 12, 0), "未到 12:00 时不应判定为已到达");
    }

    #[test]
    fn backup_moment_reached_on_exact_time() {
        let now = at(2026, 9, 16, 12, 0);
        assert!(reached(now, 12, 0), "正好等于兜底时刻应判定为已到达");
    }

    #[test]
    fn backup_moment_reached_after_target() {
        let now = at(2026, 9, 16, 15, 0);
        assert!(reached(now, 12, 0), "已过 12:00 应判定为已到达");
    }

    #[test]
    fn midnight_target_is_reached_from_start_of_day() {
        // 00:00 作兜底时刻时，当天从零点起就算「已到达」。于是启动那天不会重复兜底
        // （启动即签已覆盖），而跨天后的第一秒会因日期变化触发，每天仍恰好一次。
        assert!(reached(at(2026, 9, 16, 0, 0), 0, 0));
        assert!(reached(at(2026, 9, 16, 0, 30), 0, 0));
        assert!(reached(at(2026, 9, 16, 23, 59), 0, 0));
    }

    #[test]
    fn local_ai_defaults_to_off_and_keeps_legacy_configs_loading() {
        let config = AppConfig::default();
        assert!(!config.local_ai.enabled, "本地 AI 监控默认必须关闭");
        assert_eq!(config.local_ai.services.len(), 3, "默认监控三个服务");
        assert_eq!(config.local_ai.services[0].port, 8080);
        assert!(config.local_ai.services[0].tcp_only, "网关只能做 TCP 探测");

        // 旧版本的 config.json 没有 local_ai 字段，升级后必须仍能解析且保持关闭。
        let legacy = r#"{"auto_enabled": true, "backup_hour": 9, "backup_minute": 30}"#;
        let parsed: AppConfig = serde_json::from_str(legacy).expect("旧配置应仍可解析");
        assert!(!parsed.local_ai.enabled);
        assert_eq!(parsed.local_ai.services.len(), 3);
        assert_eq!((parsed.backup_hour, parsed.backup_minute), (9, 30));
    }

    #[test]
    fn local_ai_round_trips_through_json() {
        let mut config = AppConfig::default();
        config.local_ai.enabled = true;
        config.local_ai.log_dir = "~/custom/logs".to_string();

        let text = serde_json::to_string(&config).expect("应可序列化");
        let back: AppConfig = serde_json::from_str(&text).expect("应可反序列化");
        assert!(back.local_ai.enabled);
        assert_eq!(back.local_ai.log_dir, "~/custom/logs");
    }

    #[test]
    fn local_ai_drops_services_with_invalid_port() {
        let raw = r#"{"enabled": true, "services": [
            {"key": "a", "port": 0, "name": "坏端口"},
            {"key": "b", "port": 9999, "name": " 正常 "}
        ]}"#;
        let parsed: LocalAiConfig = serde_json::from_str(raw).expect("应可解析");
        let fixed = parsed.normalized();
        assert_eq!(fixed.services.len(), 1, "端口为 0 的服务应被剔除");
        assert_eq!(fixed.services[0].name, "正常", "名称两头空白应被裁掉");
    }

    #[test]
    fn empty_service_list_falls_back_to_defaults() {
        let raw = r#"{"services": []}"#;
        let parsed: LocalAiConfig = serde_json::from_str(raw).expect("应可解析");
        assert_eq!(parsed.normalized().services.len(), 3);
    }

    #[test]
    fn tilde_expands_to_home_and_leaves_others_alone() {
        let home = home_dir();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/a/b"), home.join("a").join("b"));
        assert_eq!(expand_tilde("  ~/a  "), home.join("a"), "两侧空白应先裁掉");
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde("rel/path"), PathBuf::from("rel/path"));
        // ~user 不做展开：猜错家目录比不展开危险得多
        assert_eq!(expand_tilde("~someone/x"), PathBuf::from("~someone/x"));
    }

    #[test]
    fn default_log_dir_is_under_home() {
        assert_eq!(default_log_dir(), home_dir().join("llama.cpp").join("logs"));
    }

    #[test]
    fn configured_smctemp_path_wins_over_convention() {
        let candidates = smctemp_candidates("~/Desktop/smctemp");
        assert_eq!(candidates[0], home_dir().join("Desktop").join("smctemp"));

        // 未配置时不应插入空路径，否则会把当前目录误当成可执行文件
        let fallback = smctemp_candidates("  ");
        assert!(!fallback.is_empty());
        assert!(fallback.iter().all(|path| path.as_os_str().len() > 0));
        assert_eq!(
            fallback[0],
            data_dir().join("bin").join("smctemp"),
            "未配置时首选应用数据目录下的副本"
        );
    }

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .expect("日期应合法")
            .and_hms_opt(hour, minute, 0)
            .expect("时间应合法")
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("路径应存在").permissions().mode() & 0o777
    }
}
