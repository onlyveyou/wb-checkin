//! Windows 界面自动化（UI Automation）。
//!
//! macOS 版走 ApplicationServices 的 AX API；Windows 对应的是 UI Automation (UIA)。
//! 对外接口与 `axdrive.rs` 保持一致，`lib.rs` 只认这组函数，平台实现在编译期二选一。
//!
//! - 进程探测 / 窗口计数：Win32（Toolhelp32 + EnumWindows），不依赖 UIA，先落地；
//! - 「召唤专家 → 写一句 → 发送」：由 UIA 驱动，接口已留好，实现在补全中。

use serde_json::{json, Value};

/// WorkBuddy / CodeBuddy 客户端在 Windows 上的可执行名候选。
const CLIENT_EXE_NAMES: &[&str] = &["WorkBuddy.exe", "CodeBuddy.exe"];

/// 客户端进程是否在跑。
///
/// 先按可执行名匹配；真正的 UIA 驱动还要拿到主窗口句柄。
pub fn client_pid() -> Option<u32> {
    // 任务 6 里接 Toolhelp32Snapshot 实现，这里先如实回报「未知」。
    let _ = CLIENT_EXE_NAMES;
    None
}

/// 授权状态 + 客户端存活。
///
/// Windows 没有 macOS 那种「辅助功能」TCC 授权：同权限级别的窗口 UIA 直接可读，
/// 所以 `trusted` 恒为 true；能不能用取决于 `windows` 是否 > 0。
pub fn status() -> Value {
    let pid = client_pid();
    json!({
        "trusted": true,
        "clientPid": pid,
        "windows": 0,
        // 前端可据此显示「Windows 版界面自动化尚在实现中」，而不是误报未授权。
        "supported": false,
    })
}

pub fn probe() -> Result<Value, String> {
    Err("Windows 版界面自动化尚在实现中".to_string())
}

pub fn open_settings() -> Result<(), String> {
    // Windows 没有「辅助功能」授权页需要跳转。
    Ok(())
}

pub fn request_trust() -> Value {
    status()
}

pub fn startup_check() {}

pub fn spawn_watcher() {}
