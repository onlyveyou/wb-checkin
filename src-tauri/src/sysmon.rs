//! 菜单栏与面板用的系统指标：硬盘、网速、CPU、内存。
//!
//! 四条读数全部走系统调用，不解析子进程输出 —— 采样线程每秒跑一次，起一次
//! `netstat` 大约 5~10 ms，叠起来比读几个内核计数器贵一个数量级。
//! 唯一例外是内存：`localai` 里已经有一套 `host_statistics64` 的实现，直接复用。

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::AppHandle;

use crate::{NET_TRAY_ID, TRAY_ID};

// ---------------------------------------------------------------------------
// 指标标识
// ---------------------------------------------------------------------------

pub const ITEM_NET: &str = "net";
pub const ITEM_DISK: &str = "disk";
pub const ITEM_CPU: &str = "cpu";
pub const ITEM_MEM: &str = "mem";

/// 可勾选的全部标识。数组顺序即菜单栏里的显示顺序，与前端勾选框一致。
pub const ITEMS: [&str; 4] = [ITEM_NET, ITEM_DISK, ITEM_CPU, ITEM_MEM];

fn bit_of(item: &str) -> u8 {
    match item {
        ITEM_NET => 1,
        ITEM_DISK => 2,
        ITEM_CPU => 4,
        ITEM_MEM => 8,
        _ => 0,
    }
}

/// 菜单栏勾选状态的位图。采样线程每秒读一次、改配置时写一次，用原子量就够，
/// 不值得为一行配置上锁。
static MENUBAR_MASK: AtomicU8 = AtomicU8::new(1 | 2);

/// 把勾选项收敛成位图。非法标识直接丢弃，不报错。
pub fn set_items(items: &[String]) {
    let mut mask = 0u8;
    for item in items {
        mask |= bit_of(item);
    }
    MENUBAR_MASK.store(mask, Ordering::Relaxed);
}

/// 当前勾选的标识（保持 `ITEMS` 的固定顺序）。
pub fn selected() -> Vec<&'static str> {
    let mask = MENUBAR_MASK.load(Ordering::Relaxed);
    ITEMS
        .iter()
        .copied()
        .filter(|item| mask & bit_of(item) != 0)
        .collect()
}

/// 归一化一份配置里的勾选项：去重、排序、剔除未知值。
pub fn normalize(items: &[String]) -> Vec<String> {
    selected_from(items).iter().map(|item| item.to_string()).collect()
}

fn selected_from(items: &[String]) -> Vec<&'static str> {
    ITEMS
        .iter()
        .copied()
        .filter(|known| items.iter().any(|item| item == known))
        .collect()
}

// ---------------------------------------------------------------------------
// 硬盘
// ---------------------------------------------------------------------------

/// 根分区的「总量 / 已用」字节数。
///
/// APFS 的多个卷共享容器空间，`statvfs` 给出的可用量是整个容器口径，所以这里
/// 的百分比是「本机磁盘用了多少」，不是单个卷的配额占用。
#[cfg(target_os = "macos")]
pub fn disk() -> Option<(u64, u64)> {
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: 路径是以 NUL 结尾的静态字节串；`stat` 是栈上变量，由内核按
    // `statvfs` 的定义整块写入。
    let rc = unsafe { libc::statvfs(b"/\0".as_ptr() as *const libc::c_char, &mut stat) };
    if rc != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    if block == 0 {
        return None;
    }
    let total = (stat.f_blocks as u64).saturating_mul(block);
    let free = (stat.f_bfree as u64).saturating_mul(block);
    (total > 0).then(|| (total, total.saturating_sub(free)))
}

// ---------------------------------------------------------------------------
// 网速
// ---------------------------------------------------------------------------

/// `struct if_data` 里 `ifi_ibytes` / `ifi_obytes` 的字节偏移。
///
/// 前 8 个字段是 `u_char`（type / typelen / physical / addrlen / hdrlen /
/// recvquota / xmitquota / unused1），占 8 字节；随后 8 个 `u_int32_t`
/// （mtu / metric / baudrate / ipackets / ierrors / opackets / oerrors /
/// collisions），ibytes 落在 8 + 8*4 = 40，obytes 紧随其后。
/// 两个偏移在本机用 ctypes 直接读 `getifaddrs` 的 `ifa_data` 与 `netstat -ibn`
/// 对照过，数值一致。
const IF_DATA_IBYTES: usize = 40;
const IF_DATA_OBYTES: usize = 44;

/// `sa_family` 在 `sockaddr` 里的偏移。macOS 的 `sockaddr` 以 `sa_len` 开头，
/// 不像 Linux 那样 `sa_family` 打头 —— 按 Linux 习惯读第 0 字节会永远读不到
/// `AF_LINK`，结果是网速恒为 0。
const SOCKADDR_FAMILY_OFFSET: usize = 1;
const AF_LINK: u8 = 18;

/// 全部物理接口的累计收发字节数。
///
/// 计数器是 32 位的，超过 4 GB 会回绕；调用方用差值时必须用 `wrapping_sub`。
/// `lo0` 排除在外，否则本机回环流量会让网速看起来虚高。
#[cfg(target_os = "macos")]
pub fn net_totals() -> Option<(u64, u64)> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: 按 libc 约定传入链表头指针，成功后由 `freeifaddrs` 释放。
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }

    let mut rx = 0u64;
    let mut tx = 0u64;
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = list;

    while !cursor.is_null() {
        // SAFETY: 链表节点在 freeifaddrs 之前一直有效，指针逐个前移不越界。
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;

        if entry.ifa_data.is_null() || entry.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: ifa_addr 指向内核填好的 sockaddr，前两字节固定是长度与协议族。
        let family = unsafe { *(entry.ifa_addr as *const u8).add(SOCKADDR_FAMILY_OFFSET) };
        if family != AF_LINK {
            continue;
        }

        // SAFETY: ifa_name 由 getifaddrs 保证是以 NUL 结尾的字符串。
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }
            .to_string_lossy()
            .to_string();
        // 同一接口在链表里可能出现多次（不同地址族），只算 AF_LINK 那条，
        // 但仍按名字去重，避免重复累加。
        if name.starts_with("lo") || seen.iter().any(|item| item == &name) {
            continue;
        }

        let base = entry.ifa_data as *const u8;
        // SAFETY: 偏移来自内核头文件的 if_data 布局；read_unaligned 允许非对齐访问。
        let read = |offset: usize| unsafe { (base.add(offset) as *const u32).read_unaligned() } as u64;
        rx = rx.saturating_add(read(IF_DATA_IBYTES));
        tx = tx.saturating_add(read(IF_DATA_OBYTES));
        seen.push(name);
    }

    // SAFETY: list 由 getifaddrs 分配，这里成对释放且不再使用。
    unsafe { libc::freeifaddrs(list) };
    Some((rx, tx))
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

/// `host_cpu_load_info`（`mach/host_info.h`）：四个 tick 计数。
///
/// 顺序是 user / system / idle / nice，内核按 `[u32; 4]` 整块写回。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct HostCpuLoadInfo {
    ticks: [u32; 4],
}

const HOST_CPU_LOAD_INFO: i32 = 3;
const HOST_CPU_LOAD_INFO_COUNT: u32 = 4;

#[cfg(target_os = "macos")]
extern "C" {
    fn mach_host_self() -> u32;
    fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
}

#[cfg(target_os = "macos")]
fn cpu_ticks() -> Option<[u32; 4]> {
    let mut info = HostCpuLoadInfo::default();
    let mut count = HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: `info` 的大小正好等于 count 个整数，内核不会写出边界。
    let rc = unsafe {
        host_statistics(
            mach_host_self(),
            HOST_CPU_LOAD_INFO,
            &mut info as *mut HostCpuLoadInfo as *mut i32,
            &mut count,
        )
    };
    (rc == 0).then_some(info.ticks)
}

/// 由两次 tick 快照算占用率。计数器是 32 位，长时间不采样也会回绕，
/// 因此按 `wrapping_sub` 求增量。
fn cpu_percent(prev: [u32; 4], now: [u32; 4]) -> Option<f64> {
    let mut delta = [0u32; 4];
    for index in 0..4 {
        delta[index] = now[index].wrapping_sub(prev[index]);
    }
    let total: u64 = delta.iter().map(|value| u64::from(*value)).sum();
    if total == 0 {
        return None;
    }
    let idle = u64::from(delta[2]);
    Some(((total - idle) as f64 / total as f64) * 100.0)
}

// ---------------------------------------------------------------------------
// 采样
// ---------------------------------------------------------------------------

/// 一次采样的结果。`None` 表示这次没读到，界面按「—」显示，而不是显示 0。
#[derive(Clone, Copy, Default)]
pub struct Sample {
    pub disk_total: Option<u64>,
    pub disk_used: Option<u64>,
    pub disk_percent: Option<f64>,
    pub net_down: Option<f64>,
    pub net_up: Option<f64>,
    pub cpu_percent: Option<f64>,
    pub mem_percent: Option<f64>,
}

/// 最近一次采样。前端面板每秒取一次，与菜单栏共用同一份读数 —— 两处显示
/// 同一个数字，比各自采一遍更不容易出现「菜单栏和面板对不上」的困惑。
static LAST: Mutex<Sample> = Mutex::new(Sample {
    disk_total: None,
    disk_used: None,
    disk_percent: None,
    net_down: None,
    net_up: None,
    cpu_percent: None,
    mem_percent: None,
});

impl Sample {
    fn to_json(self) -> Value {
        json!({
            "diskPercent": self.disk_percent,
            "diskTotal": self.disk_total,
            "diskUsed": self.disk_used,
            "netDown": self.net_down,
            "netUp": self.net_up,
            "cpuPercent": self.cpu_percent,
            "memPercent": self.mem_percent,
            "items": selected(),
        })
    }
}

fn percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    ((used as f64 / total as f64) * 1000.0).round() / 10.0
}

fn read_memory_percent() -> Option<f64> {
    crate::localai::read_memory()
        .get("usedPercent")
        .and_then(Value::as_f64)
}

/// 取最近一次采样。
///
/// 采样线程在启动时先采一次，正常情况下这里不会是空值；线程还没跑到时才退回
/// 即时读硬盘与内存，保证面板首帧不空。
pub fn snapshot() -> Value {
    let cached = LAST.lock().map(|guard| *guard).unwrap_or_default();
    if cached.disk_percent.is_some() {
        return cached.to_json();
    }

    let disk = self::disk();
    Sample {
        disk_total: disk.map(|(total, _)| total),
        disk_used: disk.map(|(_, used)| used),
        disk_percent: disk.map(|(total, used)| percent(used, total)),
        mem_percent: read_memory_percent(),
        ..Sample::default()
    }
    .to_json()
}

/// 字节速率格式化：`1.2M` / `300K` / `850B`。
///
/// 菜单栏空间紧张，省掉 `/s` 后缀；面板上会自行补上单位说明。
fn human_rate(rate: f64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    let mut value = rate.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// 网速那个图标的两行文字：下行在上、上行在下。
///
/// 没勾网速、或还没算出速率时返回空串，调用方会把这个图标藏起来。
/// 两行靠 `crate::menubar` 把 9pt 的富文本标题扣到状态栏按钮上实现 ——
/// Tauri 的 `set_title` 用系统默认字号，两行会被裁掉（实测第一行溢出到屏幕上方）。
pub fn net_title(sample: &Sample) -> String {
    if !selected().contains(&ITEM_NET) {
        return String::new();
    }
    match (sample.net_down, sample.net_up) {
        (Some(down), Some(up)) => format!("↓{}\n↑{}", human_rate(down), human_rate(up)),
        _ => String::new(),
    }
}

/// 主图标上的文字：硬盘、CPU、内存排成一行。
///
/// 网速不在这里 —— 它占两行，混进来会把硬盘百分比挤到第二行去。
pub fn info_title(sample: &Sample) -> String {
    let mut parts: Vec<String> = Vec::new();

    for item in selected() {
        match item {
            ITEM_DISK => {
                if let Some(value) = sample.disk_percent {
                    parts.push(format!("HD {value:.0}%"));
                }
            }
            ITEM_CPU => {
                if let Some(value) = sample.cpu_percent {
                    parts.push(format!("CPU {value:.0}%"));
                }
            }
            ITEM_MEM => {
                if let Some(value) = sample.mem_percent {
                    parts.push(format!("MEM {value:.0}%"));
                }
            }
            _ => {}
        }
    }

    parts.join(" · ")
}

/// 认领状态栏按钮用的一次性标记。必须足够独特，且不会出现在真实文字里。
const MARK_NET: &str = "wb#net";
const MARK_INFO: &str = "wb#info";

/// 给某个托盘刷文字。
///
/// 富文本标题没法按 id 直接寻址到按钮，所以先给托盘打一个独特的纯文本标记，
/// `menubar` 按这个标记认领按钮后立刻换成 9pt 富文本 —— 两步在同一轮事件循环里
/// 完成，标记不会被真的画出来。认领失败就退回单行纯文本，至少不会留空白。
fn paint(app: &AppHandle, tray_id: &str, mark: &str, text: &str) {
    let Some(tray) = app.tray_by_id(tray_id) else {
        return;
    };
    if text.is_empty() {
        let _ = tray.set_title(Some(""));
        return;
    }

    let _ = tray.set_title(Some(mark));
    if crate::menubar::apply_marked(mark, text) {
        return;
    }

    let flat = text.replace('\n', " ");
    let _ = tray.set_title(Some(flat.as_str()));
}

/// 起常驻采样线程：每秒读一次计数、刷新菜单栏标题。
///
/// 线程不随窗口开关启停 —— 窗口收起后菜单栏仍要显示数据，这正是它存在的意义。
pub fn spawn(app: AppHandle) {
    // 启动时把配置里的勾选项灌进位图，否则会用到默认值。
    let configured = crate::paths::load_config().menubar_items;
    set_items(&configured);

    std::thread::spawn(move || {
        let mut previous_net = net_totals();
        let mut previous_cpu = cpu_ticks();
        let mut previous_at = Instant::now();
        let mut last_net = String::new();
        let mut last_info = String::new();
        let mut first = true;

        loop {
            // 第一轮不睡：面板一打开就该看到硬盘与内存读数，网速差一秒才有值。
            if !first {
                std::thread::sleep(Duration::from_secs(1));
            }
            first = false;

            let tick_at = Instant::now();
            let elapsed = tick_at.duration_since(previous_at).as_secs_f64().max(0.001);
            previous_at = tick_at;

            let disk = disk();
            let mut sample = Sample {
                disk_total: disk.map(|(total, _)| total),
                disk_used: disk.map(|(_, used)| used),
                disk_percent: disk.map(|(total, used)| percent(used, total)),
                mem_percent: read_memory_percent(),
                ..Sample::default()
            };

            if let Some((rx, tx)) = net_totals() {
                if let Some((prev_rx, prev_tx)) = previous_net {
                    // 32 位计数器回绕时 wrapping_sub 给出的仍是正确增量。
                    let down = rx.wrapping_sub(prev_rx) as f64 / elapsed;
                    let up = tx.wrapping_sub(prev_tx) as f64 / elapsed;
                    sample.net_down = Some(down);
                    sample.net_up = Some(up);
                }
                previous_net = Some((rx, tx));
            }

            if let Some(ticks) = cpu_ticks() {
                if let Some(previous) = previous_cpu {
                    sample.cpu_percent = cpu_percent(previous, ticks);
                }
                previous_cpu = Some(ticks);
            }

            if let Ok(mut guard) = LAST.lock() {
                *guard = sample;
            }

            let net = net_title(&sample);
            let info = info_title(&sample);
            // 文字没变就不碰托盘，省掉一次不必要的界面重绘。
            if net != last_net || info != last_info {
                // AppKit 只能在主线程动，采样线程算完把文字丢过去。
                let handle = app.clone();
                let (net_next, info_next) = (net.clone(), info.clone());
                let _ = app.run_on_main_thread(move || {
                    // 网速图标只是显示用，没内容就整个藏起来，不占菜单栏位置。
                    if let Some(tray) = handle.tray_by_id(NET_TRAY_ID) {
                        if net_next.is_empty() {
                            let _ = tray.set_visible(false);
                        } else {
                            // 勾回来时状态栏项是新建的，按钮也得重新认领一次。
                            let _ = tray.set_visible(true);
                            paint(&handle, NET_TRAY_ID, MARK_NET, &net_next);
                        }
                    }

                    // 主图标始终留着，它是这个后台进程唯一的入口。
                    paint(&handle, TRAY_ID, MARK_INFO, &info_next);
                });
                last_net = net;
                last_info = info;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Windows 实现
//
// 与 macOS 逐项对应：GetDiskFreeSpaceExW / GetIfTable2 / GetSystemTimes。
// 计数器口径保持一致（net 返回累计字节、cpu_ticks 第 3 位是 idle），
// 上层 `snapshot` 与托盘绘制不用区分平台。
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
pub fn disk() -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    // 系统盘用 %SystemDrive%（默认 C:），拼成宽字符路径交给 W 版接口。
    let root = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let path = format!("{root}\\");
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    let mut free_to_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    // SAFETY: wide 以 NUL 结尾；三个出参都是栈上 u64。
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_to_caller, &mut total, &mut total_free)
    };
    if ok == 0 || total == 0 {
        return None;
    }
    Some((total, total.saturating_sub(total_free)))
}

#[cfg(target_os = "windows")]
pub fn net_totals() -> Option<(u64, u64)> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, MIB_IF_TABLE2,
    };

    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: 成功后 table 由系统分配，必须用 FreeMibTable 释放。
    if unsafe { GetIfTable2(&mut table) } != 0 || table.is_null() {
        return None;
    }

    let mut rx = 0u64;
    let mut tx = 0u64;
    // SAFETY: NumEntries 与紧随其后的 Table 数组由 API 保证一致。
    unsafe {
        let entries = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), entries);
        for row in rows {
            // 本机回环不算（对应 mac 侧排除 lo0），否则网速会虚高。
            const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK {
                continue;
            }
            rx = rx.saturating_add(row.InOctets);
            tx = tx.saturating_add(row.OutOctets);
        }
        FreeMibTable(table as *const _);
    }
    Some((rx, tx))
}

#[cfg(target_os = "windows")]
fn cpu_ticks() -> Option<[u32; 4]> {
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    let mut idle = std::mem::zeroed();
    let mut kernel = std::mem::zeroed();
    let mut user = std::mem::zeroed();
    // SAFETY: 三个出参都是 FILETIME 栈变量。
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }
    // FILETIME 是 64 位 100ns 计数。上层用 wrapping_sub 求差值，取低 32 位即可。
    let idle_ticks = idle.dwLowDateTime;
    let kernel_ticks = kernel.dwLowDateTime;
    let user_ticks = user.dwLowDateTime;
    // Windows 的 kernel 时间已包含 idle，必须减掉，否则 idle 被重复计入。
    let system = kernel_ticks.wrapping_sub(idle_ticks);
    Some([user_ticks, system, idle_ticks, 0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_of_maps_every_known_item() {
        assert_eq!(bit_of(ITEM_NET), 1);
        assert_eq!(bit_of(ITEM_DISK), 2);
        assert_eq!(bit_of(ITEM_CPU), 4);
        assert_eq!(bit_of(ITEM_MEM), 8);
        assert_eq!(bit_of("bogus"), 0);
    }

    #[test]
    fn normalize_drops_unknown_and_restores_order() {
        let input = vec!["mem".to_string(), "bogus".to_string(), "disk".to_string()];
        assert_eq!(normalize(&input), vec!["disk".to_string(), "mem".to_string()]);
    }

    #[test]
    fn human_rate_switches_units_at_1024() {
        assert_eq!(human_rate(0.0), "0B");
        assert_eq!(human_rate(1023.0), "1023B");
        assert_eq!(human_rate(1024.0), "1.0K");
        assert_eq!(human_rate(1536.0), "1.5K");
        assert_eq!(human_rate(1024.0 * 1024.0), "1.0M");
        assert_eq!(human_rate(1024.0 * 1024.0 * 1024.0), "1.0G");
    }

    #[test]
    fn cpu_percent_uses_wrapping_delta() {
        // 总增量 4 个 tick，其中 idle 3 个，占用应为 25%
        let prev = [10, 10, 10, 0];
        let now = [11, 10, 13, 0];
        let value = cpu_percent(prev, now).expect("应有结果");
        assert!((value - 25.0).abs() < 0.01, "实际 {value}");
    }

    #[test]
    fn cpu_percent_handles_counter_wrap() {
        // 计数器从 u32::MAX 附近回绕，增量仍应为 10
        let prev = [u32::MAX - 4, 0, 0, 0];
        let now = [5, 0, 0, 0];
        let value = cpu_percent(prev, now).expect("应有结果");
        assert!((value - 100.0).abs() < 0.01, "全部 tick 都在 user 上，占用应为 100%，实际 {value}");
    }

    #[test]
    fn disk_reads_a_plausible_root_volume() {
        let (total, used) = disk().expect("应能读到根分区");
        assert!(total > 1024 * 1024 * 1024, "总量异常小: {total}");
        assert!(used <= total, "已用 {used} 超过总量 {total}");
    }

    #[test]
    fn net_totals_returns_a_reading() {
        let (rx, tx) = net_totals().expect("应能读到网卡计数");
        assert!(rx > 0 || tx > 0, "收发计数不应同时为 0");
    }

    #[test]
    fn titles_split_between_two_trays() {
        set_items(&["disk".to_string()]);
        let sample = Sample {
            disk_percent: Some(72.4),
            net_down: Some(1024.0 * 1024.0),
            net_up: Some(300.0 * 1024.0),
            cpu_percent: Some(35.0),
            mem_percent: Some(61.0),
            ..Sample::default()
        };
        assert_eq!(info_title(&sample), "HD 72%");
        // 网速不再混进主图标：它自己占一个图标，两行分别是下行和上行。
        assert_eq!(net_title(&sample), "");

        set_items(&[ITEM_NET.to_string(), ITEM_MEM.to_string()]);
        assert_eq!(net_title(&sample), "↓1.0M\n↑300.0K");
        assert_eq!(info_title(&sample), "MEM 61%");

        set_items(&[ITEM_NET.to_string()]);
        assert_eq!(net_title(&sample), "↓1.0M\n↑300.0K");
        // 只勾网速时主图标上不留文字，只剩图标本身当入口
        assert_eq!(info_title(&sample), "");
    }
}
