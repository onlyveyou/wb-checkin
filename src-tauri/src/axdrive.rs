//! 用辅助功能（Accessibility）驱动 WorkBuddy 客户端。
//!
//! 为什么是原生绑定而不是 `osascript`：实测本 App 已经在系统设置的「辅助功能」
//! 列表里勾选，但由它拉起的 `osascript` 子进程查到的仍是自己的授权（`windows=0`）。
//! 只有进程本身是 App 才能用上这份授权，所以这里直接链 ApplicationServices。
//!
//! 全部走裸 C API：需要的就十来个函数，为一件事引一整套 objc2 绑定不划算。
//! 内存管理按 Core Foundation 的规矩来 —— `Copy` / `Create` 出来的对象由调用方
//! 释放，`Get` 出来的不释放。这里的封装把「取到即释放」收在内部，外部只碰 [`Node`]。

use std::ffi::CString;
use std::os::raw::{c_char, c_void};

use serde_json::{json, Value};

type AXUIElementRef = *const c_void;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;

const KCF_ENCODING_UTF8: u32 = 0x0800_0100;
/// `AXValueGetValue` 的类型标记。
const KAX_VALUE_CG_POINT: u32 = 1;
const KAX_VALUE_CG_SIZE: u32 = 2;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: libc::pid_t) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut CFArrayRef) -> i32;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> i32;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetLength(string: CFStringRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        string: CFStringRef,
        buffer: *mut c_char,
        size: isize,
        encoding: u32,
    ) -> bool;
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> *const c_void;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    fn CFGetTypeID(value: CFTypeRef) -> usize;
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFRetain(value: CFTypeRef) -> CFTypeRef;
    fn CFRelease(value: *const c_void);

    /// CFDictionary 的标准回调表。`CFDictionaryCreate` 传 NULL 回调时 key 按指针比较，
    /// AX 方用自己的 `CFSTR` 常量去查就查不到 —— 它随后对 NULL 调 `CFGetTypeID`，直接段错误。
    /// 这里只要符号地址，结构内容不读，所以声明成不透明块。
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    /// `const CFBooleanRef kCFBooleanTrue;` —— 符号本身存的就是指针，读一次就是单例。
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
}

// 合成键盘事件：webview 里的输入框不认 `AXValue` 写入，只能真的敲。
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateKeyboardEvent(source: *const c_void, key: u16, down: bool) -> *const c_void;
    fn CGEventSetFlags(event: *const c_void, flags: u64);
    fn CGEventPost(tap: u32, event: *const c_void);
    fn CGEventSourceCreate(state: u32) -> *const c_void;
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        mouse_type: u32,
        point: CGPoint,
        button: u32,
    ) -> *const c_void;
}

/// `kCGHIDEventTap`，投递到当前聚焦的窗口。
const CG_HID_EVENT_TAP: u32 = 0;
/// `kCGEventFlagMaskCommand`
const CG_FLAG_COMMAND: u64 = 1 << 20;
/// 键码 9 = `v`。
const KEY_V: u16 = 9;
/// `kCGEventFlagMaskShift`
const CG_FLAG_SHIFT: u64 = 1 << 17;

/// 把 `enter` / `cmd+a` 这类写法翻成键码 + 修饰键。
fn parse_key(spec: &str) -> Option<(u16, u64)> {
    let mut parts: Vec<&str> = spec.split('+').map(|part| part.trim()).collect();
    let key = parts.pop()?;
    let mut flags = 0u64;
    for modifier in parts {
        match modifier.to_lowercase().as_str() {
            "cmd" | "command" => flags |= CG_FLAG_COMMAND,
            "shift" => flags |= CG_FLAG_SHIFT,
            _ => return None,
        }
    }
    let code = match key.to_lowercase().as_str() {
        "enter" | "return" => 36,
        "esc" | "escape" => 53,
        "backspace" | "delete" => 51,
        "tab" => 48,
        "space" => 49,
        "up" => 126,
        "down" => 125,
        "left" => 123,
        "right" => 124,
        "a" => 0,
        "v" => KEY_V,
        other => {
            let ch = other.chars().next()?;
            match ch {
                'a'..='z' => {
                    // 字母键码在 macOS 上不是连续的字母序，只支持常用几个
                    match ch {
                        'c' => 8,
                        'x' => 7,
                        'f' => 3,
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
    };
    Some((code, flags))
}

fn send_key(spec: &str) -> bool {
    let Some((code, flags)) = parse_key(spec) else {
        return false;
    };
    unsafe {
        let source = CGEventSourceCreate(0);
        for down in [true, false] {
            let event = CGEventCreateKeyboardEvent(source, code, down);
            if event.is_null() {
                return false;
            }
            if flags != 0 {
                CGEventSetFlags(event, flags);
            }
            CGEventPost(CG_HID_EVENT_TAP, event);
            CFRelease(event);
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        if !source.is_null() {
            CFRelease(source);
        }
    }
    true
}

/// 只用来取地址的不透明块，尺寸给足以免链接期对齐问题。
#[repr(C)]
struct Opaque([usize; 8]);

unsafe impl Sync for Opaque {}

// ---------------------------------------------------------------------------
// 小工具：CFString 的创建/读取
// ---------------------------------------------------------------------------

/// CFString 只用于当参数传，用完立刻释放，所以做成「借了就走」的形式。
struct CFStr(CFStringRef);

impl CFStr {
    fn new(text: &str) -> Option<Self> {
        let c = CString::new(text).ok()?;
        let raw = unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), KCF_ENCODING_UTF8) };
        if raw.is_null() {
            None
        } else {
            Some(Self(raw))
        }
    }

    fn raw(&self) -> CFStringRef {
        self.0
    }
}

impl Drop for CFStr {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

/// 把一个 CFType 包成「离开作用域就释放」，避免递归里漏掉。
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn as_text(value: CFTypeRef) -> String {
    if value.is_null() {
        return String::new();
    }
    unsafe {
        if CFGetTypeID(value) != CFStringGetTypeID() {
            return String::new();
        }
        let length = CFStringGetLength(value);
        if length <= 0 {
            return String::new();
        }
        let capacity = CFStringGetMaximumSizeForEncoding(length, KCF_ENCODING_UTF8) + 1;
        let mut buffer = vec![0i8; capacity as usize];
        if CFStringGetCString(value, buffer.as_mut_ptr(), capacity, KCF_ENCODING_UTF8) {
            std::ffi::CStr::from_ptr(buffer.as_ptr())
                .to_string_lossy()
                .into_owned()
        } else {
            String::new()
        }
    }
}

fn copy_attr(element: AXUIElementRef, name: &str) -> Option<Owned> {
    let key = CFStr::new(name)?;
    let mut out: CFTypeRef = std::ptr::null();
    let code = unsafe { AXUIElementCopyAttributeValue(element, key.raw(), &mut out) };
    if code == 0 && !out.is_null() {
        Some(Owned(out))
    } else {
        None
    }
}

fn attr_text(element: AXUIElementRef, name: &str) -> String {
    copy_attr(element, name).map(|owned| as_text(owned.0)).unwrap_or_default()
}

/// 取子元素。**每个元素都 retain 一次**再用 [`Owned`] 管生命周期 ——
/// 数组在本函数返回时就释放了，若不 retain，调用方拿到的是悬空指针（曾经因此段错误）。
fn children(element: AXUIElementRef) -> Vec<Owned> {
    let Some(array) = copy_attr(element, "AXChildren") else {
        return Vec::new();
    };
    unsafe {
        if CFGetTypeID(array.0) != CFArrayGetTypeID() {
            return Vec::new();
        }
        let count = CFArrayGetCount(array.0);
        let mut out = Vec::with_capacity(count.max(0) as usize);
        for index in 0..count {
            let child = CFArrayGetValueAtIndex(array.0, index) as CFTypeRef;
            if !child.is_null() {
                out.push(Owned(CFRetain(child)));
            }
        }
        out
    }
}

fn actions(element: AXUIElementRef) -> Vec<String> {
    let mut out: CFArrayRef = std::ptr::null();
    let code = unsafe { AXUIElementCopyActionNames(element, &mut out) };
    if code != 0 || out.is_null() {
        return Vec::new();
    }
    // Copy 出来的由我们释放；元素是 CFString，直接取文本即可，不单独 retain
    let names = unsafe {
        let count = CFArrayGetCount(out);
        (0..count)
            .map(|index| as_text(CFArrayGetValueAtIndex(out, index) as CFTypeRef))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
    };
    unsafe { CFRelease(out) };
    names
}

fn box_of(element: AXUIElementRef) -> Option<(i64, i64, i64, i64)> {
    let position = copy_attr(element, "AXPosition")?;
    let size = copy_attr(element, "AXSize")?;
    let mut point = CGPoint::default();
    let mut extent = CGSize::default();
    let ok_point = unsafe {
        AXValueGetValue(
            position.0,
            KAX_VALUE_CG_POINT,
            &mut point as *mut CGPoint as *mut c_void,
        )
    };
    let ok_size = unsafe {
        AXValueGetValue(
            size.0,
            KAX_VALUE_CG_SIZE,
            &mut extent as *mut CGSize as *mut c_void,
        )
    };
    if !ok_point || !ok_size {
        return None;
    }
    Some((
        point.x.round() as i64,
        point.y.round() as i64,
        extent.width.round() as i64,
        extent.height.round() as i64,
    ))
}

/// 只数个数，不 retain —— 递归里每个节点都会问一次，别为计数做无谓的 retain/release。
fn child_count(element: AXUIElementRef) -> usize {
    let Some(array) = copy_attr(element, "AXChildren") else {
        return 0;
    };
    unsafe {
        if CFGetTypeID(array.0) != CFArrayGetTypeID() {
            return 0;
        }
        CFArrayGetCount(array.0).max(0) as usize
    }
}

// ---------------------------------------------------------------------------
// 元素描述
// ---------------------------------------------------------------------------

/// dump 出来的一行。坐标/尺寸全给，因为 Electron 的标题常常是空的，
/// 认不出名字时只能靠位置点。
#[derive(Clone)]
pub struct Node {
    pub role: String,
    pub title: String,
    pub desc: String,
    pub identifier: String,
    pub value: String,
    pub frame: Option<(i64, i64, i64, i64)>,
    pub actions: Vec<String>,
    pub kids: usize,
}

impl Node {
    fn line(&self, depth: usize) -> String {
        let mut text = format!("{}{}", "  ".repeat(depth), self.role);
        if !self.title.is_empty() {
            text.push_str(&format!(" T=[{}]", self.title));
        }
        if !self.desc.is_empty() {
            text.push_str(&format!(" D=[{}]", self.desc));
        }
        if !self.identifier.is_empty() {
            text.push_str(&format!(" I=[{}]", self.identifier));
        }
        if !self.value.is_empty() {
            let shown: String = self.value.chars().take(80).collect();
            let suffix = if self.value.chars().count() > 80 { "…" } else { "" };
            text.push_str(&format!(" V=[{shown}{suffix}]"));
        }
        if self.kids > 0 {
            text.push_str(&format!(" kids={}", self.kids));
        }
        if let Some((x, y, w, h)) = self.frame {
            text.push_str(&format!(" @{x},{y} {w}x{h}"));
        }
        if !self.actions.is_empty() {
            text.push_str(&format!(" acts={}", self.actions.join("|")));
        }
        text
    }

    fn interesting(&self) -> bool {
        !self.title.is_empty()
            || !self.desc.is_empty()
            || !self.identifier.is_empty()
            || !self.value.is_empty()
    }
}

fn describe(element: AXUIElementRef) -> Node {
    Node {
        role: attr_text(element, "AXRole"),
        title: attr_text(element, "AXTitle"),
        desc: attr_text(element, "AXDescription"),
        identifier: attr_text(element, "AXIdentifier"),
        value: attr_text(element, "AXValue"),
        frame: box_of(element),
        actions: actions(element),
        kids: child_count(element),
    }
}

// ---------------------------------------------------------------------------
// 客户端进程
// ---------------------------------------------------------------------------

/// 客户端主进程的 pid。只认主 bundle，渲染/网络/GPU 子进程挂的是 WebKit 的 id。
pub fn client_pid() -> Option<u32> {
    let output = std::process::Command::new("pgrep")
        .arg("-f")
        .arg("WorkBuddy.app/Contents/MacOS")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().next()?.parse::<u32>().ok()
}

fn app_element(pid: u32) -> Option<AXUIElementRef> {
    let element = unsafe { AXUIElementCreateApplication(pid as libc::pid_t) };
    if element.is_null() {
        None
    } else {
        Some(element)
    }
}

/// Electron 渲染子进程的 pid。
///
/// Chromium 把 web 内容的辅助功能树挂在自己身上，主进程那边只有原生外壳，
/// 所以想认输入框、发送按钮这些网页控件，必须去看渲染进程。
fn renderer_pid() -> Option<u32> {
    let parent = client_pid()?;
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let pid = parts.next()?.parse::<u32>().ok()?;
        let ppid = parts.next()?.parse::<u32>().ok()?;
        if ppid != parent || !line.contains("--type=renderer") {
            return None;
        }
        Some(pid)
    })
}

/// 切模型。`workbuddy://switch-model?modelId=…` 是客户端自己支持的 deeplink，
/// 比去点下拉菜单稳 —— 菜单里模型的顺序、文案都会随账号权益变。
///
/// `hy3` 是「限时免费」那档（0.00x）；`hy4` 只在夜间免费，白天照常计费。
pub fn switch_model(model_id: &str) -> Result<(), String> {
    if model_id.trim().is_empty() {
        return Ok(());
    }
    std::process::Command::new("open")
        .arg("-a")
        .arg("/Applications/WorkBuddy.app")
        .arg(format!("workbuddy://switch-model?modelId={}", model_id.trim()))
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("切模型失败：{error}"))
}

/// 点一个元素的中心。图标按钮没有文案，但 AX 给了坐标，按坐标点比猜位置稳。
fn click_element(element: AXUIElementRef) -> bool {
    let Some((x, y, w, h)) = box_of(element) else {
        return false;
    };
    if w <= 0 || h <= 0 {
        return false;
    }
    click_at(x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0)
}

// ---------------------------------------------------------------------------
// 对外命令
// ---------------------------------------------------------------------------

/// 授权状态 + 客户端存活 + 是否真读得到窗口。
///
/// `trusted` 只说明系统设置里勾没勾；`windows` 才是能用的证据。
pub fn status() -> Value {
    let trusted = unsafe { AXIsProcessTrusted() };
    let pid = client_pid();
    let mut windows = 0usize;
    if let Some(pid) = pid {
        if let Some(app) = app_element(pid) {
            let wins = copy_attr(app, "AXWindows");
            if let Some(wins) = wins {
                unsafe {
                    if CFGetTypeID(wins.0) == CFArrayGetTypeID() {
                        windows = CFArrayGetCount(wins.0).max(0) as usize;
                    }
                }
            }
            // AXUIElement 也是 CFType，Create 出来的要释放
            unsafe { CFRelease(app) };
        }
    }
    json!({ "trusted": trusted, "clientPid": pid, "windows": windows })
}

/// 主动弹一次系统授权提示。
///
/// 之前这里崩过：`CFDictionaryCreate` 传 NULL 回调时字典按指针比 key，
/// `AXIsProcessTrustedWithOptions` 查不到自己的 key 就会拿 NULL 去 `CFGetTypeID`，
/// 段错误（`KERN_INVALID_ADDRESS at 0x8`）。必须传标准回调表。
pub fn request_trust() -> Value {
    unsafe {
        let prompt = build_prompt_options();
        if let Some((options, key)) = prompt {
            // 先确认字典里真能查到，查不到就别交给 AX，免得它崩。
            let looked_up = CFDictionaryGetValue(options, key.raw());
            if !looked_up.is_null() && CFGetTypeID(looked_up) == CFBooleanGetTypeID() {
                let _ = AXIsProcessTrustedWithOptions(options);
            }
            CFRelease(options);
        }
    }
    let _ = open_settings();
    status()
}

/// 构造 `{ AXTrustedCheckOptionPrompt: true }`。返回值带上 key，方便调用方自查一次。
unsafe fn build_prompt_options() -> Option<(CFDictionaryRef, CFStr)> {
    let key = CFStr::new("AXTrustedCheckOptionPrompt")?;
    let value = kCFBooleanTrue;
    if value.is_null() || CFGetTypeID(value) != CFBooleanGetTypeID() {
        return None;
    }
    let keys = [key.raw()];
    let values = [value];
    let options = CFDictionaryCreate(
        std::ptr::null(),
        keys.as_ptr(),
        values.as_ptr(),
        1,
        &kCFTypeDictionaryKeyCallBacks as *const Opaque as *const c_void,
        &kCFTypeDictionaryValueCallBacks as *const Opaque as *const c_void,
    );
    if options.is_null() {
        None
    } else {
        Some((options, key))
    }
}

/// Chromium 系的 webview 默认**不建**辅助功能树（省内存）。`AXManualAccessibility`
/// 是 Electron/Chromium 认的开关，设完之后窗口里才出现真正的控件节点。
fn enable_manual_accessibility(app: AXUIElementRef) -> bool {
    set_attr_bool(app, "AXManualAccessibility", true)
}

fn set_attr_bool(element: AXUIElementRef, name: &str, value: bool) -> bool {
    let Some(key) = CFStr::new(name) else {
        return false;
    };
    let raw = unsafe { if value { kCFBooleanTrue } else { kCFBooleanFalse } };
    if raw.is_null() {
        return false;
    }
    unsafe { AXUIElementSetAttributeValue(element, key.raw(), raw) == 0 }
}

#[allow(dead_code)]
fn set_attr_text(element: AXUIElementRef, name: &str, text: &str) -> bool {
    let (Some(key), Some(value)) = (CFStr::new(name), CFStr::new(text)) else {
        return false;
    };
    unsafe { AXUIElementSetAttributeValue(element, key.raw(), value.raw()) == 0 }
}

fn press(element: AXUIElementRef) -> bool {
    let Some(action) = CFStr::new("AXPress") else {
        return false;
    };
    unsafe { AXUIElementPerformAction(element, action.raw()) == 0 }
}

/// dump 客户端窗口树到 `ax-dump.txt`。
pub fn probe() -> Result<Value, String> {
    probe_with(false)
}

/// `full=true` 时连没有文案的节点一起打 —— 输入框、图标按钮平时都被过滤掉了，
/// 而它们恰恰是要点的东西。
pub fn probe_with(full: bool) -> Result<Value, String> {
    let Some(pid) = client_pid() else {
        return Err("没找到 WorkBuddy 客户端进程，先把它打开".to_string());
    };
    dump_pid(pid, full, "ax-dump.txt")
}

/// dump 指定进程的窗口树。
///
/// 主进程只暴露原生外壳（侧边栏、弹层），web 内容在 Electron 的渲染子进程里，
/// 要认输入框这类控件得看渲染进程的树。
pub fn dump_pid(pid: u32, full: bool, file: &str) -> Result<Value, String> {
    let trusted = unsafe { AXIsProcessTrusted() };
    let Some(app) = app_element(pid) else {
        return Err("拿不到该进程的辅助功能句柄".to_string());
    };

    let manual = enable_manual_accessibility(app);
    if manual {
        // 树是懒建的，设完开关要给它一点时间铺开，否则读到的还是空壳。
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("trusted={trusted}"));
    lines.push(format!("client pid={pid}"));
    lines.push(format!("manualAccessibility={manual}"));

    let mut windows = 0usize;
    if let Some(wins) = copy_attr(app, "AXWindows") {
        unsafe {
            if CFGetTypeID(wins.0) == CFArrayGetTypeID() {
                let count = CFArrayGetCount(wins.0);
                windows = count.max(0) as usize;
                for index in 0..count {
                    let win = CFArrayGetValueAtIndex(wins.0, index) as AXUIElementRef;
                    if win.is_null() {
                        continue;
                    }
                    lines.push(format!("===== window {index} ====="));
                    walk(win, 0, full, &mut lines);
                }
            }
        }
    }
    lines.push(format!("windows={windows}"));
    unsafe { CFRelease(app) };

    let text = lines.join("\n");
    let out = crate::paths::data_dir().join(file);
    crate::paths::write_private(&out, &text)
        .map_err(|error| format!("写 dump 失败：{error}"))?;
    Ok(json!({
        "path": out.display().to_string(),
        "lines": lines.len(),
        "windows": windows,
        "head": lines.iter().take(40).cloned().collect::<Vec<_>>().join("\n"),
    }))
}

fn walk(element: AXUIElementRef, depth: usize, full: bool, lines: &mut Vec<String>) {
    const MAX_DEPTH: usize = 18;
    const MAX_LINES: usize = 8000;
    if depth > MAX_DEPTH || lines.len() > MAX_LINES {
        return;
    }
    let node = describe(element);
    if full || node.interesting() || depth <= 3 {
        lines.push(node.line(depth));
    }
    for child in children(element) {
        walk(child.0, depth + 1, full, lines);
    }
}

#[allow(dead_code)]
pub fn dump_path() -> std::path::PathBuf {
    crate::paths::data_dir().join("ax-dump.txt")
}

// ---------------------------------------------------------------------------
// 元素检索与动作
// ---------------------------------------------------------------------------

/// 角色 + 关键词同时限定。`role` 为空表示不限角色，`needle` 为空表示不限文案
/// —— 图标按钮通常没有任何文案，只能靠角色 + 位置认。
fn hits(node: &Node, role: &str, needle: &str) -> bool {
    if !role.is_empty() && !node.role.eq_ignore_ascii_case(role) {
        return false;
    }
    if needle.is_empty() {
        return true;
    }
    matches(node, needle)
}

/// 一个节点是否命中关键词。Electron 的节点常常没有 title，description /
/// identifier 反而有内容，四个字段一起比。
fn matches(node: &Node, needle: &str) -> bool {
    let needle = needle.to_lowercase();
    node.title.to_lowercase().contains(&needle)
        || node.desc.to_lowercase().contains(&needle)
        || node.identifier.to_lowercase().contains(&needle)
        || node.value.to_lowercase().contains(&needle)
}

/// 深度优先找第一个命中的节点，返回 retain 过的句柄。
fn find_first(
    element: AXUIElementRef,
    role: &str,
    needle: &str,
    depth: usize,
) -> Option<Owned> {
    if depth > 18 {
        return None;
    }
    if hits(&describe(element), role, needle) {
        let kept = unsafe { CFRetain(element) };
        return Some(Owned(kept));
    }
    for child in children(element) {
        if let Some(found) = find_first(child.0, role, needle, depth + 1) {
            return Some(found);
        }
    }
    None
}

/// 列出所有命中节点（最多 20 个），调试选择器时看这个比看整棵树快。
fn find_all(
    element: AXUIElementRef,
    role: &str,
    needle: &str,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth > 18 || out.len() >= 40 {
        return;
    }
    let node = describe(element);
    if hits(&node, role, needle) {
        out.push(node.line(depth));
    }
    for child in children(element) {
        find_all(child.0, role, needle, depth + 1, out);
    }
}

/// 把文本塞进剪贴板再按 Cmd+V。webview 的输入框不认 `AXValue` 写入，
/// 而逐字符合成键盘事件又会丢掉中文，粘贴是唯一稳的路子。
fn paste_text(text: &str) -> bool {
    use std::io::Write;
    let child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        if stdin.write_all(text.as_bytes()).is_err() {
            return false;
        }
    }
    let _ = child.wait();
    // 给剪贴板一点时间落地，否则粘贴到的是上一份内容
    std::thread::sleep(std::time::Duration::from_millis(120));
    unsafe {
        let source = CGEventSourceCreate(0);
        for down in [true, false] {
            let event = CGEventCreateKeyboardEvent(source, KEY_V, down);
            if event.is_null() {
                return false;
            }
            CGEventSetFlags(event, CG_FLAG_COMMAND);
            CGEventPost(CG_HID_EVENT_TAP, event);
            CFRelease(event);
        }
        if !source.is_null() {
            CFRelease(source);
        }
    }
    true
}

// ---------------------------------------------------------------------------
// 指令文件：让调试不必每次重编译
// ---------------------------------------------------------------------------

pub fn command_path() -> std::path::PathBuf {
    crate::paths::data_dir().join("ax-cmd.txt")
}

fn log_path() -> std::path::PathBuf {
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

/// 后台每 0.7 秒看一眼 `ax-cmd.txt`。文件里写 `key=value` 若干行，
/// `action` 决定做什么，跑完把结果追加到 `ax-log.txt` 并删掉指令文件。
///
/// 这是给「调驱动逻辑」用的窄口子：只认下面列出的几个动作，不接受任意命令。
pub fn spawn_watcher() {
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_millis(700));
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
    let mut fields = std::collections::HashMap::new();
    for line in raw.lines() {
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    let action = fields.get("action").cloned().unwrap_or_default();
    let needle = fields.get("needle").cloned().unwrap_or_default();
    let role = fields.get("role").cloned().unwrap_or_default();
    let full = fields.get("full").map(|v| v == "1").unwrap_or(false);
    // 不写 pid 就用主进程；写 `pid=renderer` 走渲染进程，那里才有 web 控件。
    let pid: Option<u32> = match fields.get("pid").map(String::as_str) {
        Some("renderer") => renderer_pid(),
        Some(other) => other.parse::<u32>().ok(),
        None => client_pid(),
    };
    match action.as_str() {
        "probe" => {
            let Some(pid) = pid else {
                return "找不到目标进程".to_string();
            };
            let file = if fields.contains_key("pid") {
                format!("ax-dump-{pid}.txt")
            } else {
                "ax-dump.txt".to_string()
            };
            match dump_pid(pid, full, &file) {
                Ok(value) => format!(
                    "pid={pid} dump {} 行，windows={}，{}",
                    value["lines"], value["windows"], value["path"]
                ),
                Err(error) => format!("失败：{error}"),
            }
        }
        // 点元素中心：图标按钮没文案但 AX 有坐标。
        "clickel" => {
            let Some(pid) = pid else {
                return "找不到目标进程".to_string();
            };
            let Some(app) = open_client_at(pid) else {
                return "拿不到辅助功能句柄".to_string();
            };
            match find_first(app.0, &role, &needle, 0) {
                Some(target) => {
                    let frame = box_of(target.0);
                    let ok = click_element(target.0);
                    format!("clickel({role}/{needle}) frame={frame:?} = {ok}")
                }
                None => format!("没找到「{role}/{needle}」"),
            }
        }
        "switch" => {
            let model = fields.get("model").cloned().unwrap_or_default();
            match switch_model(&model) {
                Ok(()) => format!("已切到 {model}"),
                Err(error) => error,
            }
        }
        "find" => {
            let Some(app) = pid.and_then(open_client_at) else {
                return "找不到客户端".to_string();
            };
            let mut found = Vec::new();
            find_all(app.0, &role, &needle, 0, &mut found);
            if found.is_empty() {
                "没找到".to_string()
            } else {
                format!("\n{}", found.join("\n"))
            }
        }
        "press" => {
            let Some(app) = pid.and_then(open_client_at) else {
                return "找不到客户端".to_string();
            };
            match find_first(app.0, &role, &needle, 0) {
                Some(target) => format!("press({role}/{needle}) = {}", press(target.0)),
                None => format!("没找到「{role}/{needle}」"),
            }
        }
        "paste" => {
            let Some(app) = open_client() else {
                return "找不到客户端".to_string();
            };
            let text = fields.get("text").cloned().unwrap_or_default();
            match find_first(app.0, &role, &needle, 0) {
                Some(target) => {
                    let focused = set_attr_bool(target.0, "AXFocused", true);
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    let pasted = paste_text(&text);
                    format!("focus={focused} paste={pasted}")
                }
                None => format!("没找到「{role}/{needle}」"),
            }
        }
        // 往当前焦点里写：先全选清空，再整段粘贴。不依赖能否在树里找到输入框。
        "type" => {
            let text = fields.get("text").cloned().unwrap_or_default();
            let clear = fields.get("clear").map(|v| v != "0").unwrap_or(true);
            if clear {
                send_key("cmd+a");
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            // 空文本就是「清空」：粘贴空内容是空操作，得用退格删掉选区。
            if text.is_empty() {
                format!("clear = {}", send_key("backspace"))
            } else {
                format!("paste = {}", paste_text(&text))
            }
        }
        "click" => {
            let x = fields.get("x").and_then(|v| v.parse::<f64>().ok());
            let y = fields.get("y").and_then(|v| v.parse::<f64>().ok());
            match (x, y) {
                (Some(x), Some(y)) => format!("click({x},{y}) = {}", click_at(x, y)),
                _ => "缺 x / y".to_string(),
            }
        }
        "open" => {
            let url = fields.get("url").cloned().unwrap_or_default();
            if !url.starts_with("workbuddy://") {
                return "只放行 workbuddy:// 链接".to_string();
            }
            match open_url(&url) {
                Ok(()) => format!("已打开 {url}"),
                Err(error) => error,
            }
        }
        // 跑一个完整的「体验专家」：切免费模型 → 打开 → 召唤 → 写一句 → 发送
        "expert" => {
            let expert = fields.get("expert").cloned().unwrap_or_default();
            let text = fields.get("text").cloned().unwrap_or_default();
            match run_expert(&expert, &text) {
                Ok(value) => format!(
                    "model={} {}",
                    value["model"].as_str().unwrap_or(""),
                    value["steps"].as_array().map(|items| items
                        .iter()
                        .filter_map(|item| item.as_str())
                        .collect::<Vec<_>>()
                        .join(" → "))
                        .unwrap_or_default()
                ),
                Err(error) => format!("失败：{error}"),
            }
        }
        // 看系统认为谁有焦点 —— 盲点之前先确认落点，比反复试坐标靠谱。
        "focused" => {
            let Some(app) = open_client() else {
                return "找不到客户端".to_string();
            };
            let mut out = Vec::new();
            for name in ["AXFocusedUIElement", "AXFocusedWindow"] {
                match copy_attr(app.0, name) {
                    Some(value) => out.push(format!("{name}: {}", describe(value.0).line(0))),
                    None => out.push(format!("{name}: 无")),
                }
            }
            // 再看一眼窗口标题，确认当前是哪个会话
            out.join("\n")
        }
        "key" => {
            let spec = fields.get("value").cloned().unwrap_or_default();
            format!("key({spec}) = {}", send_key(&spec))
        }
        "activate" => activate_client(),
        other => format!("未知动作「{other}」"),
    }
}

/// 在屏幕坐标上点一下。图标类按钮没有任何文案，认不到就只能按位置点。
fn click_at(x: f64, y: f64) -> bool {
    let point = CGPoint { x, y };
    unsafe {
        let source = CGEventSourceCreate(0);
        for kind in [5u32, 1, 2] {
            let event = CGEventCreateMouseEvent(source, kind, point, 0);
            if event.is_null() {
                return false;
            }
            CGEventPost(CG_HID_EVENT_TAP, event);
            CFRelease(event);
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        if !source.is_null() {
            CFRelease(source);
        }
    }
    true
}

/// 把客户端拉到前台，并**确认真的在前台** —— 合成的键鼠事件只会投给前台应用，
/// 本 App 自己刚被 `open` 拉起时会抢焦点，不核对就会把字打到自己的窗口里。
fn activate_client() -> String {
    let _ = std::process::Command::new("open")
        .arg("-a")
        .arg("/Applications/WorkBuddy.app")
        .spawn();
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(120));
        if frontmost() == "WorkBuddy" {
            return "已激活，前台=WorkBuddy".to_string();
        }
    }
    format!("没能切到前台，当前前台={}", frontmost())
}

/// 当前前台应用的显示名。`lsappinfo` 不需要任何授权。
fn frontmost() -> String {
    let asn = std::process::Command::new("lsappinfo")
        .arg("front")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    if asn.is_empty() {
        return String::new();
    }
    let info = std::process::Command::new("lsappinfo")
        .arg("info")
        .arg("-only")
        .arg("name")
        .arg(&asn)
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default();
    info.split('"')
        .nth(3)
        .map(|name| name.to_string())
        .unwrap_or_default()
}

/// 拿到客户端 app 元素，并确保辅助功能树已经铺开。
fn open_client() -> Option<Owned> {
    open_client_at(client_pid()?)
}

fn open_client_at(pid: u32) -> Option<Owned> {
    let app = app_element(pid)?;
    enable_manual_accessibility(app);
    Some(Owned(app))
}

/// 启动时留一次快照，装完不用点任何东西就能判断授权是否真的可用。
pub fn startup_check() {
    let _ = probe();
}

/// 用客户端自己打开一个 deeplink。
fn open_url(url: &str) -> Result<(), String> {
    std::process::Command::new("open")
        .arg("-a")
        .arg("/Applications/WorkBuddy.app")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("打开链接失败：{error}"))
}

/// 主窗口的 frame，兜底定位用。
fn main_window_frame() -> Option<(i64, i64, i64, i64)> {
    let app = open_client()?;
    let wins = copy_attr(app.0, "AXWindows")?;
    unsafe {
        if CFGetTypeID(wins.0) != CFArrayGetTypeID() || CFArrayGetCount(wins.0) == 0 {
            return None;
        }
        let win = CFArrayGetValueAtIndex(wins.0, 0) as AXUIElementRef;
        box_of(win)
    }
}

/// 输入框卡片的位置。
///
/// web 内容里输入框和发送按钮都不暴露（卡片那层的 `AXChildren` 是空的），
/// 只有卡片自己对外的 frame 是可读的，所以发送键要按卡片的内边距推算。
fn composer_frame(element: AXUIElementRef, depth: usize, best: &mut Option<(i64, i64, i64, i64)>) {
    if depth > 18 {
        return;
    }
    if let Some((x, y, w, h)) = box_of(element) {
        let plausible = x > 400 && (600..=1500).contains(&w) && (80..=240).contains(&h) && y > 200;
        if plausible {
            let area = w * h;
            // 取**最小**的那层：输入框卡片外面还套了一层（含「选择工作空间」那行），
            // 用外层算内边距会点到卡片下面的空白处。
            let better = best.map(|(_, _, bw, bh)| area < bw * bh).unwrap_or(true);
            if better {
                *best = Some((x, y, w, h));
            }
        }
    }
    for child in children(element) {
        composer_frame(child.0, depth + 1, best);
    }
}

/// 客户端当前输入框卡片的纵坐标。
///
/// 新任务页的输入框在窗口中间，发出去之后会挪到窗口底部 —— 用这个位移判断
/// 「点发送到底有没有生效」，比盯着按钮可靠。
fn client_composer_y() -> Option<i64> {
    let app = open_client()?;
    let mut frame = None;
    composer_frame(app.0, 0, &mut frame);
    frame.map(|(_, y, _, _)| y)
}

/// 侧边栏里有没有一条以 `prefix` 开头的新会话。
///
/// 发出第一条消息后客户端会在左侧建一条以提问开头的会话，这是「真的发出去了」
/// 最硬的证据 —— 光看输入框位移会被跳页骗到。
fn walk_sidebar(element: AXUIElementRef, depth: usize, prefix: &str, hit: &mut bool) {
    if depth > 18 || *hit {
        return;
    }
    let in_sidebar = box_of(element).map(|(x, _, _, _)| x < 420).unwrap_or(false);
    if in_sidebar {
        let node = describe(element);
        if node.value.starts_with(prefix) || node.title.starts_with(prefix) {
            *hit = true;
            return;
        }
    }
    for child in children(element) {
        walk_sidebar(child.0, depth + 1, prefix, hit);
    }
}

fn sidebar_has(prefix: &str) -> bool {
    if prefix.trim().is_empty() {
        return false;
    }
    let Some(app) = open_client() else {
        return false;
    };
    let mut hit = false;
    walk_sidebar(app.0, 0, prefix, &mut hit);
    hit
}

/// 点发送按钮。
///
/// 按钮是纯图标（没有文案、也不在树里），先按名字碰运气，认不到就按输入框卡片推算：
/// 实测（卡片 936x134）发送键在卡片右下角内缩 30px 处。
fn click_send() -> bool {
    for pid in [renderer_pid(), client_pid()].into_iter().flatten() {
        let Some(app) = open_client_at(pid) else {
            continue;
        };
        for needle in ["发送", "提交", "send", "Send"] {
            if let Some(target) = find_first(app.0, "AXButton", needle, 0) {
                if click_element(target.0) {
                    return true;
                }
            }
        }
    }
    let Some(app) = open_client() else {
        return false;
    };
    let mut frame = None;
    composer_frame(app.0, 0, &mut frame);
    match frame {
        Some((x, y, w, h)) => click_at((x + w - 30) as f64, (y + h - 30) as f64),
        None => match main_window_frame() {
            Some((x, y, w, h)) => click_at(x as f64 + w as f64 - 370.0, y as f64 + h as f64 / 2.0),
            None => false,
        },
    }
}

/// 跑一个「体验专家」任务。
///
/// 服务端没有进度上报接口，进度是从真实对话反推的，所以只能在客户端里真的开一个
/// 会话、真的说一句话。**新建会话前先切免费模型** —— 这种走流程的对话没必要花额度。
pub fn run_expert(expert_id: &str, prompt: &str) -> Result<Value, String> {
    let expert_id = expert_id.trim();
    if expert_id.is_empty() {
        return Err("缺少 expertId".to_string());
    }
    let model = crate::paths::load_config().free_model;
    let mut steps: Vec<String> = Vec::new();

    // 合成的键鼠只投给前台应用，本 App 刚被拉起时会抢焦点。
    // `lsappinfo` 有时会说客户端在前台、实际 key window 不是它，所以这里无条件拉一次。
    steps.push(activate_client());
    if frontmost() != "WorkBuddy" {
        return Err(format!(
            "客户端没能到前台（当前 {}），先手动点一下它",
            frontmost()
        ));
    }

    if !model.is_empty() {
        switch_model(&model)?;
        steps.push(format!("切模型 → {model}"));
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }

    open_url(&format!("workbuddy://experts?expertId={expert_id}"))?;

    // 专家详情是个弹层，打开需要时间，网络慢的时候两三秒还不够 —— 轮询等它出现。
    let mut summoned = false;
    for _ in 0..14 {
        std::thread::sleep(std::time::Duration::from_millis(700));
        let Some(app) = open_client() else {
            continue;
        };
        match find_first(app.0, "", "召唤", 0) {
            Some(target) => {
                summoned = press(target.0);
                if summoned {
                    break;
                }
            }
            None => continue,
        }
    }
    if !summoned {
        return Err("没找到「召唤」，专家详情可能没打开".to_string());
    }
    steps.push("已召唤".to_string());
    std::thread::sleep(std::time::Duration::from_millis(2600));

    if !prompt.trim().is_empty() {
        // 召唤出来的新任务页会预填一段示例提问，先全选再粘贴，否则会接在示例后面。
        send_key("cmd+a");
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !paste_text(prompt) {
            return Err("写入正文失败".to_string());
        }
        steps.push("已写入正文".to_string());
        std::thread::sleep(std::time::Duration::from_millis(900));
    }

    // 发送：点之前把客户端拉到前台（窗口没拿到焦点时首点会被吞），
    // 点完看侧边栏有没有多出这条会话 —— 那才是真发出去了。
    let prefix: String = prompt.trim().chars().take(6).collect();
    let before = client_composer_y();
    let mut sent = false;
    for attempt in 0..3 {
        if attempt > 0 {
            steps.push(format!("第 {} 次补点", attempt + 1));
            activate_client();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if !click_send() {
            continue;
        }
        std::thread::sleep(std::time::Duration::from_millis(1800));
        if sidebar_has(&prefix) {
            sent = true;
            break;
        }
        // 侧边栏可能还没刷新，退一步看输入框有没有从中间挪到底部。
        if let (Some(old), Some(new)) = (before, client_composer_y()) {
            if (new - old).abs() > 150 {
                steps.push("会话已建（侧边栏未刷新）".to_string());
                sent = true;
                break;
            }
        }
    }
    if !sent {
        return Err("点了发送但界面没变化，可能被弹层挡住了".to_string());
    }
    steps.push("已发送".to_string());
    Ok(json!({ "model": model, "steps": steps }))
}

/// 跳到系统设置的辅助功能面板。
pub fn open_settings() -> Result<(), String> {
    std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("打开系统设置失败：{error}"))
}
