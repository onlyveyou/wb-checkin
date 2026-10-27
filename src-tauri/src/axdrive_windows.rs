//! Windows 界面自动化（UI Automation）。
//!
//! macOS 版走 ApplicationServices 的 AX API；Windows 对应的是 UI Automation (UIA)。
//! 对外接口与 `axdrive.rs` 保持一致，`lib.rs` 只认这组函数，平台实现在编译期二选一。
//!
//! 与 macOS 版的差异（这些差异由平台决定，不是实现偷懒）：
//!
//! - **不需要系统授权**：UIA 对同权限级别的窗口直接可读，没有 TCC 那套东西。
//!   所以 `trusted` 恒为 true，能不能用只看客户端进程与窗口在不在。
//! - **不需要 `AXManualAccessibility`**：Chromium 在 Windows 上默认就把可访问树
//!   建出来（Electron 的 webview 会为 UIA 暴露 Document/Button/Edit 节点）。
//! - **文字输入用 Unicode 键事件**，不用剪贴板：`SendInput` 带 `KEYEVENTF_UNICODE`
//!   可逐字符输入中文，既不抢剪贴板，也不受输入法状态影响。
//! - **元素定位走 TreeWalker 遍历**，不用 `CreatePropertyCondition`：后者要手工构造
//!   VARIANT（windows crate 没给 VARIANT 实现 `From`），而遍历读的是类型化的
//!   `CurrentName` / `CurrentControlType`，少一大截坑。
//!
//! 调试口子与 macOS 版相同：往 `~/.wb-checkin/ax-cmd.txt` 写 `key=value`，
//! 后台每 0.7 秒消费一次，结果追加到 `ax-log.txt`。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use windows::Win32::Foundation::{HWND as UiHwnd, RECT as UiRect};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationTreeWalker, IUIAutomationValuePattern, UIA_InvokePatternId, UIA_ValuePatternId,
};

use windows_sys::Win32::Foundation::{
    CloseHandle, BOOL, HWND as SysHwnd, LPARAM, RECT as SysRect,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_MENU,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

/// WorkBuddy / CodeBuddy 客户端在 Windows 上的可执行名候选。
const CLIENT_EXE_NAMES: &[&str] = &["WorkBuddy.exe", "CodeBuddy.exe", "CodeBuddy CN.exe"];

/// 树遍历的节点上限。Chromium 的无障碍树动辄几千节点，不设上限会把一次查找
/// 拖成十几秒（每个节点都是跨进程读属性，且要读四次）。
const NODE_LIMIT: usize = 4000;
/// 递归深度上限。正常页面十来层，超了基本就是异常结构。
const DEPTH_LIMIT: usize = 40;

// ---------------------------------------------------------------------------
// COM 初始化
// ---------------------------------------------------------------------------

thread_local! {
    /// 每个线程只初始化一次 COM：`CoInitializeEx` 重复调用无害，但每次都调没意义。
    static COM_READY: RefCell<bool> = const { RefCell::new(false) };
    /// EnumWindows 的回调不能捕获环境，命中结果靠线程局部变量带出来。
    static ENUM_HITS: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
}

/// 在当前线程初始化 COM（MTA）。
///
/// 界面自动化的调用可能发生在不同线程（命令线程、watcher 线程），而 COM 是按线程
/// 初始化的，所以每个入口都得先过这一关。
fn com_ready() -> bool {
    COM_READY.with(|flag| {
        if *flag.borrow() {
            return true;
        }
        // SAFETY: pvReserved 必须是 NULL；返回值只用来判断成败。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_OK / S_FALSE / RPC_E_CHANGED_MODE 都算「这个线程能用 COM」。
        const S_OK: i32 = 0;
        const S_FALSE: i32 = 1;
        const RPC_E_CHANGED_MODE: i32 = -2147417850;
        let ok = matches!(hr.0, S_OK | S_FALSE | RPC_E_CHANGED_MODE);
        if ok {
            *flag.borrow_mut() = true;
        }
        ok
    })
}

fn automation() -> Result<IUIAutomation, String> {
    if !com_ready() {
        return Err("COM 初始化失败".to_string());
    }
    // SAFETY: CUIAutomation 是系统注册的类，MTA 下创建不需要消息泵。
    unsafe {
        CoCreateInstance(
            &CUIAutomation,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
        .map_err(|error| format!("创建 UI Automation 实例失败：{error}"))
    }
}

// ---------------------------------------------------------------------------
// 进程与窗口
// ---------------------------------------------------------------------------

fn wide_to_string(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|ch| *ch == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// 客户端进程是否在跑，返回第一个命中的 pid。
pub fn client_pid() -> Option<u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == (-1isize as SysHwnd) {
        return None;
    }

    let mut found = None;
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        let name = wide_to_string(&entry.szExeFile);
        if CLIENT_EXE_NAMES
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(&name))
        {
            found = Some(entry.th32ProcessID);
            break;
        }
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }

    unsafe { CloseHandle(snapshot) };
    found
}

unsafe extern "system" fn enum_proc(hwnd: SysHwnd, lparam: LPARAM) -> BOOL {
    let target = lparam as u32;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid != target {
        return 1;
    }
    // 只要「可见 + 有标题」的顶层窗口：离屏窗口、托盘辅助窗口都排除。
    if unsafe { IsWindowVisible(hwnd) } == 0 || unsafe { GetWindowTextLengthW(hwnd) } <= 0 {
        return 1;
    }
    ENUM_HITS.with(|hits| hits.borrow_mut().push(hwnd as isize));
    1
}

/// 客户端主窗口。多个候选时取面积最大的（主界面永远比弹窗大）。
fn client_window(pid: u32) -> Option<SysHwnd> {
    ENUM_HITS.with(|hits| hits.borrow_mut().clear());
    // SAFETY: 回调只读窗口属性并写线程局部变量，不跨越调用边界。
    unsafe {
        let _ = EnumWindows(Some(enum_proc), pid as LPARAM);
    }

    let candidates: Vec<isize> = ENUM_HITS.with(|hits| hits.borrow().clone());
    let mut best: Option<(isize, i64)> = None;
    for raw in candidates {
        let hwnd = raw as SysHwnd;
        let mut rect: SysRect = unsafe { std::mem::zeroed() };
        if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
            continue;
        }
        let area = ((rect.right - rect.left) as i64) * ((rect.bottom - rect.top) as i64);
        if area <= 0 {
            continue;
        }
        if best.map(|(_, best_area)| area > best_area).unwrap_or(true) {
            best = Some((raw, area));
        }
    }
    best.map(|(raw, _)| raw as SysHwnd)
}

/// 把客户端窗口提到前台。
///
/// Windows 有前台锁：非前台进程调 `SetForegroundWindow` 会被系统静默忽略。
/// 先按一下 ALT 能把自己「临时解冻」，这是通行的做法，成功率明显更高。
fn activate(hwnd: SysHwnd) -> bool {
    unsafe {
        if IsIconic(hwnd) != 0 {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        send_virtual_key(VK_MENU, true);
        send_virtual_key(VK_MENU, false);
        let ok = SetForegroundWindow(hwnd) != 0;
        if ok {
            std::thread::sleep(Duration::from_millis(180));
        }
        ok
    }
}

// ---------------------------------------------------------------------------
// 输入合成
// ---------------------------------------------------------------------------

fn key_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send_batch(items: &[INPUT]) -> bool {
    if items.is_empty() {
        return true;
    }
    // SAFETY: 数组长度与指针一致，cbSize 取结构体实际大小。
    let sent = unsafe {
        SendInput(
            items.len() as u32,
            items.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    sent as usize == items.len()
}

fn send_virtual_key(vk: VIRTUAL_KEY, down: bool) {
    let flags = if down { 0 } else { KEYEVENTF_KEYUP };
    send_batch(&[key_input(vk, 0, flags)]);
}

/// 逐字符输入任意 Unicode 文本。
///
/// `KEYEVENTF_UNICODE` 直接送字符本身，绕开键盘布局与输入法：中文、emoji 都能进
/// Chromium 的输入框，也不会碰剪贴板。
fn type_text(text: &str) -> bool {
    let mut items: Vec<INPUT> = Vec::new();
    for unit in text.encode_utf16() {
        items.push(key_input(0u16, unit, KEYEVENTF_UNICODE));
        items.push(key_input(
            0u16,
            unit,
            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
        ));
    }
    // 长文本分批送，免得一次塞太多被 SendInput 截断。
    items.chunks(512).all(send_batch)
}

/// Ctrl+A 全选。清空输入框靠「全选后输入新内容直接替换」。
fn select_all() -> bool {
    const VK_A: VIRTUAL_KEY = 0x41;
    send_batch(&[
        key_input(VK_CONTROL, 0, 0),
        key_input(VK_A, 0, 0),
        key_input(VK_A, 0, KEYEVENTF_KEYUP),
        key_input(VK_CONTROL, 0, KEYEVENTF_KEYUP),
    ])
}

fn press_backspace() -> bool {
    send_batch(&[
        key_input(VK_BACK, 0, 0),
        key_input(VK_BACK, 0, KEYEVENTF_KEYUP),
    ])
}

/// 在屏幕坐标点一下。坐标为物理像素——调用前必须已声明 DPI 感知。
fn click_at(x: i32, y: i32) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;
    unsafe {
        if SetCursorPos(x, y) == 0 {
            return false;
        }
        std::thread::sleep(Duration::from_millis(80));
        send_batch(&[
            mouse_input(MOUSEEVENTF_LEFTDOWN),
            mouse_input(MOUSEEVENTF_LEFTUP),
        ])
    }
}

// ---------------------------------------------------------------------------
// 元素读取
// ---------------------------------------------------------------------------

/// 与平台无关的矩形，避免在 windows / windows-sys 两套 RECT 之间来回转。
#[derive(Clone, Copy, PartialEq, Debug)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Rect {
    fn from_ui(rect: UiRect) -> Option<Self> {
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return None;
        }
        Some(Rect {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        })
    }

    fn area(&self) -> i64 {
        ((self.right - self.left) as i64) * ((self.bottom - self.top) as i64)
    }
}

fn element_name(element: &IUIAutomationElement) -> String {
    unsafe {
        element
            .CurrentName()
            .map(|text| text.to_string())
            .unwrap_or_default()
    }
}

fn element_role(element: &IUIAutomationElement) -> i32 {
    unsafe {
        element
            .CurrentControlType()
            .map(|id| id.0)
            .unwrap_or(0)
    }
}

/// ControlTypeId → 可读角色名（值取自 UIA_CONTROLTYPE_ID 常量）。
fn role_name(id: i32) -> &'static str {
    match id {
        50000 => "Button",
        50001 => "Calendar",
        50002 => "CheckBox",
        50003 => "ComboBox",
        50004 => "Edit",
        50005 => "Hyperlink",
        50006 => "Image",
        50007 => "ListItem",
        50008 => "List",
        50009 => "Menu",
        50010 => "MenuBar",
        50011 => "MenuItem",
        50012 => "ProgressBar",
        50013 => "RadioButton",
        50014 => "ScrollBar",
        50015 => "Slider",
        50016 => "Spinner",
        50017 => "StatusBar",
        50018 => "Tab",
        50019 => "TabItem",
        50020 => "Text",
        50021 => "ToolBar",
        50022 => "ToolTip",
        50023 => "Tree",
        50024 => "TreeItem",
        50025 => "Custom",
        50026 => "Group",
        50027 => "Thumb",
        50028 => "DataGrid",
        50029 => "DataItem",
        50030 => "Document",
        50031 => "SplitButton",
        50032 => "Window",
        50033 => "Pane",
        50034 => "Header",
        50035 => "HeaderItem",
        50036 => "Table",
        50037 => "TitleBar",
        50038 => "Separator",
        _ => "Unknown",
    }
}

fn element_rect(element: &IUIAutomationElement) -> Option<Rect> {
    unsafe { element.CurrentBoundingRectangle().ok() }.and_then(Rect::from_ui)
}

fn element_offscreen(element: &IUIAutomationElement) -> bool {
    unsafe {
        element
            .CurrentIsOffscreen()
            .map(|value| value.as_bool())
            .unwrap_or(false)
    }
}

fn element_value(element: &IUIAutomationElement) -> Option<String> {
    unsafe {
        let pattern: IUIAutomationValuePattern = element
            .GetCurrentPatternAs(UIA_ValuePatternId)
            .ok()?;
        pattern.CurrentValue().ok().map(|text| text.to_string())
    }
}

fn focus(element: &IUIAutomationElement) -> bool {
    unsafe { element.SetFocus().is_ok() }
}

/// 读按钮的 Invoke 模式点一下。比坐标点击更可靠（不受遮挡与缩放影响）。
fn invoke(element: &IUIAutomationElement) -> bool {
    unsafe {
        match element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) {
            Ok(pattern) => pattern.Invoke().is_ok(),
            Err(_) => false,
        }
    }
}

/// 点元素中心（按钮没有 Invoke 模式时的退路）。
fn click_element(element: &IUIAutomationElement) -> bool {
    let Some(rect) = element_rect(element) else {
        return false;
    };
    click_at((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2)
}

/// 扁平的树节点：只在遍历期间活着。
struct Node {
    depth: usize,
    name: String,
    role: i32,
    rect: Option<Rect>,
    offscreen: bool,
}

impl Node {
    fn line(&self) -> String {
        let rect = match self.rect {
            Some(rect) => format!(
                "[{},{},{},{}]",
                rect.left, rect.top, rect.right, rect.bottom
            ),
            None => "[-]".to_string(),
        };
        let label = if self.name.is_empty() {
            String::new()
        } else {
            format!("「{}」", self.name)
        };
        format!(
            "{}{}{} {}",
            "  ".repeat(self.depth),
            role_name(self.role),
            label,
            rect
        )
    }
}

/// 深度优先遍历整棵树。
///
/// `full = false` 时只留「有名字或有尺寸」的节点，dump 出来短很多；找按钮时用
/// `full = true` —— 图标按钮往往没名字。
fn flatten(
    walker: &IUIAutomationTreeWalker,
    root: &IUIAutomationElement,
    full: bool,
) -> (Vec<Node>, Vec<IUIAutomationElement>) {
    let mut nodes: Vec<Node> = Vec::new();
    let mut elements: Vec<IUIAutomationElement> = Vec::new();
    walk(walker, root, 0, full, &mut nodes, &mut elements);
    (nodes, elements)
}

fn walk(
    walker: &IUIAutomationTreeWalker,
    parent: &IUIAutomationElement,
    depth: usize,
    full: bool,
    nodes: &mut Vec<Node>,
    elements: &mut Vec<IUIAutomationElement>,
) {
    if depth > DEPTH_LIMIT || elements.len() >= NODE_LIMIT {
        return;
    }

    let mut child = unsafe { walker.GetFirstChildElement(parent).ok() };
    while let Some(element) = child {
        if elements.len() >= NODE_LIMIT {
            return;
        }
        let name = element_name(&element);
        let role = element_role(&element);
        let rect = element_rect(&element);
        let offscreen = element_offscreen(&element);

        if full || !name.is_empty() || rect.is_some() {
            nodes.push(Node {
                depth,
                name,
                role,
                rect,
                offscreen,
            });
            elements.push(element.clone());
        }

        walk(walker, &element, depth + 1, full, nodes, elements);
        child = unsafe { walker.GetNextSiblingElement(&element).ok() };
    }
}

/// 打开客户端窗口树：hwnd → 元素 → walker。
fn open_client() -> Result<(SysHwnd, IUIAutomationTreeWalker, IUIAutomationElement), String> {
    let pid = client_pid().ok_or_else(|| "没找到 WorkBuddy 客户端进程，先把它打开".to_string())?;
    let hwnd =
        client_window(pid).ok_or_else(|| "没找到客户端主窗口（是否最小化到托盘了？）".to_string())?;

    let automation = automation()?;
    // SAFETY: hwnd 来自刚枚举到的真实窗口；ElementFromHandle 只读。
    let root: IUIAutomationElement = unsafe { automation.ElementFromHandle(UiHwnd(hwnd)) }
        .map_err(|error| format!("拿不到窗口的 UI Automation 元素：{error}"))?;
    let walker = unsafe { automation.RawViewWalker() }
        .map_err(|error| format!("创建树遍历器失败：{error}"))?;

    Ok((hwnd, walker, root))
}

/// 在扁平结果里按角色 + 名字片段找元素，返回下标。
fn find_index(nodes: &[Node], role: &str, needle: &str) -> Option<usize> {
    nodes.iter().position(|node| {
        if node.offscreen {
            return false;
        }
        if !role.is_empty() && !role_name(node.role).eq_ignore_ascii_case(role) {
            return false;
        }
        if needle.is_empty() {
            node.rect.is_some()
        } else {
            node.name.contains(needle)
        }
    })
}

// ---------------------------------------------------------------------------
// 对外命令
// ---------------------------------------------------------------------------

/// 授权状态 + 客户端存活。
///
/// Windows 没有 macOS 那种「辅助功能」TCC 授权，所以 `trusted` 恒为 true；
/// 能不能用只看能不能拿到客户端窗口。
pub fn status() -> Value {
    let pid = client_pid();
    let windows = match pid.and_then(client_window) {
        Some(_) => 1,
        None => 0,
    };
    json!({
        "trusted": true,
        "clientPid": pid,
        "windows": windows,
        "platform": "windows",
    })
}

pub fn probe() -> Result<Value, String> {
    probe_with(false)
}

pub fn probe_with(full: bool) -> Result<Value, String> {
    let pid = client_pid().ok_or_else(|| "没找到 WorkBuddy 客户端进程，先把它打开".to_string())?;
    dump_pid(pid, full, "ax-dump.txt")
}

pub fn dump_pid(pid: u32, full: bool, file: &str) -> Result<Value, String> {
    let hwnd = client_window(pid).ok_or_else(|| "没找到客户端主窗口".to_string())?;
    let automation = automation()?;
    let root: IUIAutomationElement = unsafe { automation.ElementFromHandle(UiHwnd(hwnd)) }
        .map_err(|error| format!("拿不到窗口元素：{error}"))?;
    let walker = unsafe { automation.RawViewWalker() }
        .map_err(|error| format!("创建遍历器失败：{error}"))?;

    let (nodes, _) = flatten(&walker, &root, full);
    let mut text = String::new();
    text.push_str(&format!(
        "# pid={pid} full={full} nodes={}\n",
        nodes.len()
    ));
    for node in &nodes {
        text.push_str(&node.line());
        text.push('\n');
    }

    let path = dump_path();
    let _ = crate::paths::write_private(&path, &text);

    Ok(json!({
        "pid": pid,
        "path": path.to_string_lossy(),
        "lines": nodes.len(),
        "windows": 1,
        "file": file,
    }))
}

pub fn dump_path() -> PathBuf {
    crate::paths::data_dir().join("ax-dump.txt")
}

/// Windows 没有「辅助功能」授权页需要跳转。
pub fn open_settings() -> Result<(), String> {
    Err("Windows 不需要单独授权界面自动化".to_string())
}

pub fn request_trust() -> Value {
    status()
}

pub fn startup_check() {
    // 高 DPI 屏必须先声明感知：否则 UIA 给的矩形是物理像素、SetCursorPos 按逻辑
    // 像素走，点击会整体偏移。
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _ = com_ready();
    append_log(&format!("启动自检：{}", status()));
}

// ---------------------------------------------------------------------------
// 协议唤起 / 模型切换
// ---------------------------------------------------------------------------

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 用系统默认处理器打开链接。只放行 `workbuddy://`，避免变成任意 URL 执行器。
pub fn open_url(url: &str) -> Result<(), String> {
    if !url.starts_with("workbuddy://") {
        return Err("只放行 workbuddy:// 链接".to_string());
    }
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let operation = wide("open");
    let target = wide(url);
    // SAFETY: 字符串都以 NUL 结尾且在调用期间存活；两个目录参数允许为 NULL。
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        return Err(format!(
            "打开链接失败（ShellExecuteW 返回 {}）",
            result as isize
        ));
    }
    Ok(())
}

/// 切模型：与 macOS 一样走协议深链。
pub fn switch_model(model_id: &str) -> Result<(), String> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return Ok(());
    }
    open_url(&format!("workbuddy://switch-model?modelId={model_id}"))
}

// ---------------------------------------------------------------------------
// 指令文件（调试通道）
// ---------------------------------------------------------------------------

pub fn command_path() -> PathBuf {
    crate::paths::data_dir().join("ax-cmd.txt")
}

fn log_path() -> PathBuf {
    crate::paths::data_dir().join("ax-log.txt")
}

fn append_log(text: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let stamp = chrono::Local::now().format("%H:%M:%S");
        let _ = writeln!(file, "[{stamp}] {text}");
    }
}

/// 后台每 0.7 秒消费一次 `ax-cmd.txt`。
pub fn spawn_watcher() {
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(700));
        let path = command_path();
        if !path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        let report = run_command(&raw);
        append_log(&format!("命令 {} → {report}", raw.replace('\n', " ")));
    });
}

fn run_command(raw: &str) -> String {
    let mut fields: HashMap<String, String> = HashMap::new();
    for line in raw.lines() {
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    let action = fields.get("action").cloned().unwrap_or_default();
    let needle = fields.get("needle").cloned().unwrap_or_default();
    let role = fields.get("role").cloned().unwrap_or_default();
    let full = fields.get("full").map(|value| value == "1").unwrap_or(false);

    match action.as_str() {
        "status" => status().to_string(),
        "probe" => match probe_with(full) {
            Ok(value) => format!("dump {} 行 → {}", value["lines"], value["path"]),
            Err(error) => format!("失败：{error}"),
        },
        "find" => match open_client() {
            Ok((_, walker, root)) => {
                let (nodes, _) = flatten(&walker, &root, true);
                let hits: Vec<String> = nodes
                    .iter()
                    .filter(|node| {
                        (needle.is_empty() || node.name.contains(&needle))
                            && (role.is_empty()
                                || role_name(node.role).eq_ignore_ascii_case(&role))
                    })
                    .take(40)
                    .map(Node::line)
                    .collect();
                if hits.is_empty() {
                    "没找到".to_string()
                } else {
                    format!("\n{}", hits.join("\n"))
                }
            }
            Err(error) => error,
        },
        "press" => match press_named(&role, &needle) {
            Ok(true) => format!("press({role}/{needle}) = true"),
            Ok(false) => format!("找到「{role}/{needle}」但没点动"),
            Err(error) => error,
        },
        "clickel" => match click_named(&role, &needle) {
            Ok(true) => format!("clickel({role}/{needle}) = true"),
            Ok(false) => format!("找到「{role}/{needle}」但没点动"),
            Err(error) => error,
        },
        "type" => {
            let text = fields.get("text").cloned().unwrap_or_default();
            let clear = fields.get("clear").map(|value| value != "0").unwrap_or(true);
            if clear {
                select_all();
                std::thread::sleep(Duration::from_millis(120));
            }
            if text.is_empty() {
                format!("clear = {}", press_backspace())
            } else {
                format!("type = {}", type_text(&text))
            }
        },
        "click" => {
            let x = fields.get("x").and_then(|value| value.parse::<i32>().ok());
            let y = fields.get("y").and_then(|value| value.parse::<i32>().ok());
            match (x, y) {
                (Some(x), Some(y)) => format!("click({x},{y}) = {}", click_at(x, y)),
                _ => "缺 x / y".to_string(),
            }
        },
        "open" => {
            let url = fields.get("url").cloned().unwrap_or_default();
            match open_url(&url) {
                Ok(()) => format!("已打开 {url}"),
                Err(error) => error,
            }
        },
        "activate" => match open_client() {
            Ok((hwnd, _, _)) => format!("activate = {}", activate(hwnd)),
            Err(error) => error,
        },
        "switch" => {
            let model = fields.get("model").cloned().unwrap_or_default();
            match switch_model(&model) {
                Ok(()) => format!("已切到 {model}"),
                Err(error) => error,
            }
        },
        "expert" => {
            let expert_id = fields.get("expert").cloned().unwrap_or_default();
            let prompt = fields.get("text").cloned().unwrap_or_default();
            match run_expert(&expert_id, &prompt) {
                Ok(value) => value.to_string(),
                Err(error) => format!("失败：{error}"),
            }
        },
        "" => "缺少 action".to_string(),
        other => format!("未知动作：{other}"),
    }
}

// ---------------------------------------------------------------------------
// 高层动作
// ---------------------------------------------------------------------------

fn client_tree() -> Result<(SysHwnd, Vec<Node>, Vec<IUIAutomationElement>), String> {
    let (hwnd, walker, root) = open_client()?;
    let _ = activate(hwnd);
    let (nodes, elements) = flatten(&walker, &root, true);
    Ok((hwnd, nodes, elements))
}

fn press_named(role: &str, needle: &str) -> Result<bool, String> {
    let (_, nodes, elements) = client_tree()?;
    let Some(index) = find_index(&nodes, role, needle) else {
        return Err(format!("没找到「{role}/{needle}」"));
    };
    Ok(invoke(&elements[index]) || click_element(&elements[index]))
}

fn click_named(role: &str, needle: &str) -> Result<bool, String> {
    let (_, nodes, elements) = client_tree()?;
    let Some(index) = find_index(&nodes, role, needle) else {
        return Err(format!("没找到「{role}/{needle}」"));
    };
    Ok(click_element(&elements[index]))
}

/// 「召唤」按钮的名字候选。中文界面是「召唤」，英文界面可能是 Call / Summon。
const SUMMON_NEEDLES: &[&str] = &["召唤", "Call", "Summon"];
/// 发送按钮的名字候选。
const SEND_NEEDLES: &[&str] = &["发送", "Send", "提交", "Submit"];

/// 找一个名字命中候选的按钮。
///
/// 先按 `Button` 角色找（最准），找不到再放宽到任意角色 —— Chromium 里带图标的
/// 按钮有时被报成 `Custom` 或 `Pane`，只认角色会漏。
fn find_button(nodes: &[Node], needles: &[&str]) -> Option<usize> {
    for needle in needles {
        if let Some(index) = find_index(nodes, "Button", needle) {
            return Some(index);
        }
        if let Some(index) = nodes
            .iter()
            .position(|node| !node.offscreen && node.name.contains(needle) && node.rect.is_some())
        {
            return Some(index);
        }
    }
    None
}

/// 找输入框：优先 `Edit`，其次 `Document`（Chromium 的 contenteditable 常报成
/// Document）。有多个时取面积最大的 —— 页面里的搜索框都比 composer 小得多。
fn find_composer(nodes: &[Node]) -> Option<usize> {
    let mut best: Option<(usize, i64)> = None;
    for (index, node) in nodes.iter().enumerate() {
        if node.offscreen {
            continue;
        }
        let role = role_name(node.role);
        if !(role.eq_ignore_ascii_case("Edit") || role.eq_ignore_ascii_case("Document")) {
            continue;
        }
        let Some(rect) = node.rect else { continue };
        if best
            .map(|(_, best_area)| rect.area() > best_area)
            .unwrap_or(true)
        {
            best = Some((index, rect.area()));
        }
    }
    best.map(|(index, _)| index)
}

/// 发送校验用的现场快照：输入框的矩形 + 能读到的内容。
#[derive(PartialEq)]
struct ComposerState {
    rect: Option<Rect>,
    text: Option<String>,
}

impl ComposerState {
    fn capture(nodes: &[Node], elements: &[IUIAutomationElement]) -> Self {
        let Some(index) = find_composer(nodes) else {
            return ComposerState {
                rect: None,
                text: None,
            };
        };
        ComposerState {
            rect: nodes[index].rect,
            text: element_value(&elements[index]),
        }
    }

    /// 发出消息后输入框会被清空、卡片整体上移，两者都是强信号。
    fn looks_sent(&self, before: &Self) -> bool {
        if let (Some(before), Some(after)) = (before.rect, self.rect) {
            if (after.top - before.top).abs() > 60 {
                return true;
            }
        }
        self.text
            .as_deref()
            .map(|text| text.trim().is_empty())
            .unwrap_or(false)
    }
}

/// 全链路：切免费模型 → 打开专家 → 召唤 → 写一句 → 发送并校验。
///
/// 与 macOS 版同样的顺序，只把 AX 动作换成 UIA：
/// 1. 先把客户端提到前台（否则合成输入会打到别的窗口上）；
/// 2. 切模型（协议深链）后重新提一次前台 —— 协议唤起可能带起别的窗口；
/// 3. 打开专家深链，轮询等「召唤」按钮出现（弹层要几秒才加载）；
/// 4. 召唤、聚焦输入框、全选清空预填内容、输入自己的话；
/// 5. 点发送，用「输入框是否位移/清空」判断到底发出去没有，没成功补点两次。
pub fn run_expert(expert_id: &str, prompt: &str) -> Result<Value, String> {
    let expert_id = expert_id.trim();
    if expert_id.is_empty() {
        return Err("缺少 expert id".to_string());
    }
    let prompt = if prompt.trim().is_empty() {
        "你好，请用一句话介绍一下你自己。"
    } else {
        prompt.trim()
    };

    let model = crate::paths::load_config().free_model;
    let started = Instant::now();

    let pid = client_pid().ok_or_else(|| "没找到 WorkBuddy 客户端进程".to_string())?;
    let hwnd = client_window(pid).ok_or_else(|| "没找到客户端主窗口".to_string())?;
    activate(hwnd);

    if !model.trim().is_empty() {
        switch_model(&model)?;
        std::thread::sleep(Duration::from_millis(600));
        let _ = activate(hwnd);
    }

    open_url(&format!("workbuddy://experts?expertId={expert_id}"))?;

    // 弹层加载 3~10 秒，每 0.7 秒看一次「召唤」按钮
    let mut summoned = false;
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..14 {
        std::thread::sleep(Duration::from_millis(700));
        let Ok((_, walker, root)) = open_client() else {
            continue;
        };
        let (nodes, elements) = flatten(&walker, &root, true);
        if let Some(index) = find_button(&nodes, SUMMON_NEEDLES) {
            append_log(&format!("召唤按钮命中：{}", nodes[index].line()));
            let pressed = invoke(&elements[index]) || click_element(&elements[index]);
            if !pressed {
                return Err("找到「召唤」按钮但点不动（可能被其它窗口挡住）".to_string());
            }
            summoned = true;
            break;
        }
        seen = nodes
            .iter()
            .filter(|node| !node.name.is_empty())
            .take(12)
            .map(|node| node.name.clone())
            .collect();
    }
    if !summoned {
        return Err(format!(
            "等了约 10 秒没等到「召唤」按钮，界面上看到的是：{}",
            seen.join(" / ")
        ));
    }

    // 召唤会把草稿会话拉起来，给界面一点时间切过去
    std::thread::sleep(Duration::from_millis(1500));

    let (_, nodes, elements) = client_tree()?;
    let Some(composer) = find_composer(&nodes) else {
        return Err("没找到输入框（对话页可能还没加载出来）".to_string());
    };
    if !focus(&elements[composer]) {
        // 聚焦失败就退回坐标点击输入框中心
        let _ = click_element(&elements[composer]);
        std::thread::sleep(Duration::from_millis(250));
    }
    select_all();
    std::thread::sleep(Duration::from_millis(150));
    if !type_text(prompt) {
        return Err("输入文字失败".to_string());
    }
    std::thread::sleep(Duration::from_millis(400));

    let mut attempts = 0;
    let mut sent = false;
    while attempts < 3 && !sent {
        attempts += 1;

        let (nodes, elements) = {
            let (_, nodes, elements) = client_tree()?;
            (nodes, elements)
        };
        let before = ComposerState::capture(&nodes, &elements);

        let clicked = match find_button(&nodes, SEND_NEEDLES) {
            Some(index) => invoke(&elements[index]) || click_element(&elements[index]),
            // 找不到具名按钮就按「输入框右下角内缩 30px」点一下：
            // 发送键是实心圆，固定在输入卡片右下角。
            None => match before.rect {
                Some(rect) => click_at(rect.right - 30, rect.bottom - 30),
                None => false,
            },
        };
        if !clicked {
            let _ = activate(hwnd);
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }

        std::thread::sleep(Duration::from_millis(1400));
        let after = {
            let (_, nodes, elements) = client_tree()?;
            ComposerState::capture(&nodes, &elements)
        };
        sent = after.looks_sent(&before);
        if !sent {
            let _ = activate(hwnd);
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    if !sent {
        return Err("点了发送但界面没变化（会话可能没创建成功）".to_string());
    }

    Ok(json!({
        "ok": true,
        "expert": expert_id,
        "model": model,
        "attempts": attempts,
        "elapsedMs": started.elapsed().as_millis() as u64,
    }))
}
