//! 直接改菜单栏按钮的富文本标题。
//!
//! Tauri 的 `TrayIcon::set_title` 只接受纯文本，字号固定为系统默认的 13pt ——
//! 而菜单栏按钮只有 22pt 高，两行 13pt 文字会被裁掉（第一行溢出到屏幕上方）。
//! tray-icon 内部持有 `NSStatusItem`，但没有把 `button` 暴露出来，所以这里
//! 顺着 `NSApp.windows` 找到 `NSStatusBarWindow` → contentView → `NSStatusBarButton`，
//! 直接给它设 9pt / 10pt 行高的 attributedTitle —— 两行合计 20pt，正好塞得下。
//!
//! 认领按钮靠「一次性哨兵」：调用方先用 `set_title` 打一个独特的纯文本标记，
//! 这里按标题文字找到对应按钮再换成富文本。**必须在同一轮事件循环里完成**，
//! 否则哨兵会被真的画到菜单栏上。这样不需要维护按钮指针，托盘被重建也不会失联。

#![cfg(target_os = "macos")]

use std::ffi::CStr;

use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::msg_send;

/// 字号。9pt 是实测能塞进两行的上限，再大第一行就会顶出菜单栏。
const FONT_SIZE: f64 = 9.0;
/// 行高必须显式压住：默认行高约 1.2 倍字号，两行加起来仍会超。
const LINE_HEIGHT: f64 = 10.0;
/// 多行标题的基线偏移（负值 = 往下压）。
///
/// `NSStatusBarButton` 在绘制多行 `attributedTitle` 时整体偏上：实测菜单栏 30px 高，
/// 字形距顶 2px、距底 11px，底部空出一大块。往下压 5pt 后上下留白变成 6~7px，
/// 视觉上才居中。**单行绝对不能加**，否则会明显偏下。
const BASELINE_OFFSET: f64 = -5.0;
/// NSFontWeightMedium
const FONT_WEIGHT_MEDIUM: f64 = 0.23;
/// NSTextAlignmentCenter
const TEXT_ALIGN_CENTER: isize = 1;

/// 把当前标题为 `sentinel` 的菜单栏按钮改成富文本标题；找不到就返回 false。
pub fn apply_marked(sentinel: &str, text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    autoreleasepool(|_| unsafe {
        let mut button: *mut AnyObject = std::ptr::null_mut();
        walk_buttons(|candidate| {
            if string_equals(msg_send![candidate, title], sentinel) {
                button = candidate;
                return true;
            }
            false
        });
        if button.is_null() {
            return false;
        }

        let font: *mut AnyObject = msg_send![
            objc2::class!(NSFont),
            systemFontOfSize: FONT_SIZE,
            weight: FONT_WEIGHT_MEDIUM
        ];
        let color: *mut AnyObject = msg_send![objc2::class!(NSColor), labelColor];

        let paragraph: *mut AnyObject = msg_send![objc2::class!(NSMutableParagraphStyle), new];
        let _: () = msg_send![paragraph, setMinimumLineHeight: LINE_HEIGHT];
        let _: () = msg_send![paragraph, setMaximumLineHeight: LINE_HEIGHT];
        let _: () = msg_send![paragraph, setAlignment: TEXT_ALIGN_CENTER];
        let _: () = msg_send![paragraph, setLineBreakMode: 2isize]; // NSLineBreakByTruncatingTail

        let attributes: *mut AnyObject = msg_send![objc2::class!(NSMutableDictionary), dictionary];
        set_object(attributes, font, "NSFont");
        set_object(attributes, color, "NSColor");
        set_object(attributes, paragraph, "NSParagraphStyle");
        if text.contains('\n') {
            let offset: *mut AnyObject =
                msg_send![objc2::class!(NSNumber), numberWithDouble: BASELINE_OFFSET];
            set_object(attributes, offset, "NSBaselineOffset");
        }

        let allocated: *mut AnyObject = msg_send![objc2::class!(NSAttributedString), alloc];
        let string: *mut AnyObject =
            msg_send![allocated, initWithString: nsstring(text), attributes: attributes];
        if string.is_null() {
            return false;
        }

        let _: () = msg_send![button, setAttributedTitle: string];
        true
    })
}

/// 遍历所有状态栏按钮。`NSApp.windows` 里状态栏项以 `NSStatusBarWindow` 出现，
/// 它的 contentView 是 `NSStatusBarContentView`，按钮是其子视图。
unsafe fn walk_buttons(mut visit: impl FnMut(*mut AnyObject) -> bool) {
    let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
    if app.is_null() {
        return;
    }
    let windows: *mut AnyObject = msg_send![app, windows];
    if windows.is_null() {
        return;
    }

    let count: usize = msg_send![windows, count];
    for index in 0..count {
        let window: *mut AnyObject = msg_send![windows, objectAtIndex: index];
        if window.is_null() || !class_name_contains(window, "StatusBar") {
            continue;
        }
        let view: *mut AnyObject = msg_send![window, contentView];
        if visit_button(view, 0, &mut visit) {
            return;
        }
    }
}

unsafe fn visit_button(
    view: *mut AnyObject,
    depth: usize,
    visit: &mut impl FnMut(*mut AnyObject) -> bool,
) -> bool {
    // 状态栏层级很浅，限深既够用又能防意外结构导致无限下钻。
    if view.is_null() || depth > 4 {
        return false;
    }
    if class_name_contains(view, "Button") && visit(view) {
        return true;
    }

    let subviews: *mut AnyObject = msg_send![view, subviews];
    if subviews.is_null() {
        return false;
    }
    let count: usize = msg_send![subviews, count];
    for index in 0..count {
        let child: *mut AnyObject = msg_send![subviews, objectAtIndex: index];
        if visit_button(child, depth + 1, visit) {
            return true;
        }
    }
    false
}

unsafe fn class_name_contains(object: *mut AnyObject, needle: &str) -> bool {
    let name: *mut AnyObject = msg_send![object, className];
    if name.is_null() {
        return false;
    }
    let bytes: *const std::os::raw::c_char = msg_send![name, UTF8String];
    if bytes.is_null() {
        return false;
    }
    CStr::from_ptr(bytes).to_string_lossy().contains(needle)
}

unsafe fn string_equals(value: *mut AnyObject, expected: &str) -> bool {
    if value.is_null() {
        return false;
    }
    let bytes: *const std::os::raw::c_char = msg_send![value, UTF8String];
    if bytes.is_null() {
        return false;
    }
    CStr::from_ptr(bytes).to_string_lossy() == expected
}

unsafe fn set_object(dictionary: *mut AnyObject, value: *mut AnyObject, key: &str) {
    let _: () = msg_send![dictionary, setObject: value, forKey: nsstring(key)];
}

unsafe fn nsstring(text: &str) -> *mut AnyObject {
    let owned = std::ffi::CString::new(text.replace('\0', "")).unwrap_or_default();
    msg_send![objc2::class!(NSString), stringWithUTF8String: owned.as_ptr()]
}
