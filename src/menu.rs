//! 托盘设备菜单：**单一分层窗口**自绘全部像素。
//!
//! 结构（2026-11-03 重写，取代"系统菜单 + 独立动效浮层"那一版）：
//!
//! * 菜单本体就是一张 `WS_POPUP + WS_EX_LAYERED + WS_EX_TOOLWINDOW + WS_EX_TOPMOST`
//!   窗口，位置、尺寸、完整画面由 `UpdateLayeredWindow` **一次原子提交**；
//! * `layer`（32 位 DIB）里 GDI 先画底色/悬停/分隔线/标记/文字（**原始色**），同一趟
//!   逐像素合成再把这一块就地换成预乘 BGRA：圆角覆盖率乘进 alpha，当前行的扫光按
//!   亮度缩放，动画绿点在同一个循环里 src-over。只有一张像素面（旧版 scratch 已合并
//!   进来），所以当前行每帧都要先画回原始色再合成；
//! * 没有第二个窗口、没有抓屏快照、没有块掩码、没有 `SetWindowRgn`——旧版所有"猜哪一层
//!   像素是谁的"逻辑在这里不存在了，所以也不会有"直角黑块""把快速设置贴进来"这类问题。
//!
//! 交互与层序：
//!
//! * **不**加 `WS_EX_NOACTIVATE`。菜单显示后 `SetForegroundWindow`，ESC / 方向键 / 回车
//!   走正常键盘语义，点外部靠 `WM_ACTIVATE(WA_INACTIVE)` / `WM_KILLFOCUS` 关闭，
//!   点击菜单内部由本窗口自己命中行（按下记录、抬起取项）。NOACTIVATE 的窗拿不到前台，
//!   `SetCapture` 也必失败，等于放弃 ESC 与点外关闭——这正是上一版浮层被迫做成
//!   点击穿透的原因。
//! * 每帧（以及首次显示）都重申一次 `HWND_TOPMOST`。Win11 快速设置、任务栏溢出浮出窗
//!   都在 TOPMOST 带里，建窗先后并不能保证谁在上；显式重申才稳定。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::size_of;
use std::rc::Rc;

use crate::diag;
use crate::model::Entry;
use crate::naming;
use crate::tray::{
    draw_mark, fill_rect, glyph_bitmap, mark_geometry, menu_font, menu_is_dark, menu_palette,
    menu_row_height, text_width, Mark, Palette, EXIT_ROW_PX, MARK_GAP, MENU_GAP, MENU_PAD_L,
    MENU_PAD_R, MENU_RADIUS, MENU_SEPARATOR_PX, MENU_TEXT_RISE,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE,
    WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, DrawTextW, GetMonitorInfoW,
    GetTextMetricsW, MonitorFromPoint, SelectObject, SetBkColor, SetBkMode, SetTextColor,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    DT_LEFT, DT_SINGLELINE, DT_VCENTER, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, OPAQUE, TEXTMETRICW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    AttachThreadInput, CreateWaitableTimerExW, GetCurrentThreadId, SetWaitableTimer,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_SPACE,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetForegroundWindow, GetMessageTime, GetWindowRect,
    GetWindowThreadProcessId, LoadCursorW, MsgWaitForMultipleObjectsEx, PeekMessageW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, SetWindowPos, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, UpdateLayeredWindow, HHOOK, HICON, IDC_ARROW, MSG,
    MSLLHOOKSTRUCT, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, ULW_ALPHA, WA_INACTIVE, WH_MOUSE_LL, WM_ACTIVATE, WM_APP,
    WM_DESTROY, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MOUSEMOVE, WM_QUIT, WM_RBUTTONDOWN, WNDCLASSW, WNDCLASS_STYLES, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const FRAME_MS: u32 = 33;

/// 绿点帧表跨菜单复用：连续开关时不必每次重烘（烘焙 ~2.2 ms + 一次 106 KB 分配）。
const DOT_CACHE_MAX_MS: u64 = 30_000;
/// 菜单活得比这还短，才算"连续开关"，收起时保留帧表待下次复用。
const DOT_CACHE_RAPID_MS: u64 = 1_500;

/// "该收尾了"的自唤醒消息（`WM_MENU_DONE` 由 [`finish`] 投递一次）。
///
/// **这是关闭路径的关键**：关闭信号 `WM_ACTIVATE(WA_INACTIVE)` / `WM_KILLFOCUS` 是别的
/// 线程**发**（sent）过来的，由消息 API 内部直接派发给窗口过程，不会作为"取到的消息"
/// 返回给循环。`finish()` 因此必须自己投一条 posted 消息把等待中的循环叫醒——真机取证
/// 里不投这一下的等待是 10.5 s（另一次 63 s 没醒）。
const WM_MENU_DONE: u32 = WM_APP + 0x20;

/// 菜单外框。旧系统菜单把条目内缩 3 px（经典边框带）；这里保留同样的 3 px 只作视觉留白，
/// 让文字/标记的绝对落位与旧版完全一致，但不再有系统画的 1 px 浅灰边框。
const MENU_FRAME: i32 = 3;

/// 黑条左右内缩：`MENU_FRAME` + 6 = 9 px——不是消除"灰边"，而是刻意多留，做成下陷的边。
const BAND_INSET_X: i32 = MENU_FRAME + 6;

/// 黑条上下内缩：4 px，给上下两条内阴影壁留出可画的边。
const BAND_INSET_Y: i32 = 4;

/// 内阴影壁长：槽的上下边缘各自 8 px 缓升到槽底色，两侧**等长**（高度不足时夹到半高）。
const BAND_WALL_PX: i32 = 8;

/// 当前行黑条的圆角：菜单圆角 12 − 上下内缩 4 = 8 px。
///
/// 上一版取 `MENU_RADIUS − BAND_INSET_X`（= 3）：弧长只有 4.7 px、整条只有 16 个半覆盖像素，
/// 真机上读成直角（2026-11-05 反馈）。再往前取 12 也不行：黑条左右内缩 9 px，槽角圆心落在
/// (21,19)、菜单角圆心在 (12,12)，两段同半径圆弧叠成横跨约 21 px 的复合挖角，首行上角比
/// 下半边的菜单角大一圈（2026-11-04 反馈）。
///
/// 8 px 居中：槽角圆心 (17,15) 与菜单角圆心 (12,12) 相距 √(5²+3²)=5.8 px，弧长 12.6 px、
/// 四角各约 30 个半覆盖像素——既读得出圆角，也不再把首行上角放大。
const BAND_RADIUS: i32 = MENU_RADIUS - BAND_INSET_Y;

// ---------- 扫光（数值原样沿用 2026-11-02 定稿） ----------

const PERIOD_MS: u64 = 1538;
const SIGMA_RATIO: f64 = 0.18;
const DARK_OUTSIDE: f64 = 0.62;
const DARK_INSIDE: f64 = 1.0;
const LIGHT_OUTSIDE: f64 = 1.0;
const LIGHT_INSIDE: f64 = 0.28;

// ---------- 绿点（1700 ms 总相位，数值原样沿用 2026-11-02 第五版） ----------

const RIPPLE_RING_MS: u64 = 1000;
const RIPPLE_GROW_MS: u64 = 850;
const RIPPLE_IDLE_MS: u64 = 700;
const RIPPLE_CYCLE_MS: u64 = RIPPLE_RING_MS + RIPPLE_IDLE_MS;
/// 帧表按**时间**索引：每帧固定 [`FRAME_MS`]，帧数由周期整除得出（1700 ms → 51 帧）。
///
/// 旧值 64 帧 × (1700/64 = 26.6 ms 相位) 配 33 ms 定时器，会把 1700 ms 的相位拉成 2112 ms（1.24×），
/// 且相位采样点永远对不上 33 ms 的整数倍。现在帧号 i 就代表 `i * 33 ms`。
const RIPPLE_FRAMES: usize = (RIPPLE_CYCLE_MS / FRAME_MS as u64) as usize;
/// 一整个帧表覆盖的显示时长（= 帧数 × 33 ms = 1683 ms，比设计值短 1%）。
const RIPPLE_TABLE_MS: u64 = RIPPLE_FRAMES as u64 * FRAME_MS as u64;
const DOT_RGB: [u8; 3] = [18, 217, 127];
const DOT_CORE_RATIO: f64 = 0.26;
const DOT_CORE_TROUGH: f64 = 0.19;
const DOT_CORE_ALPHA: f64 = 1.0;
const DOT_RING_START: f64 = 0.48;
const DOT_RING_END: f64 = 0.85;
const DOT_RING_ALPHA: f64 = 1.0;
const DOT_RING_THICK_START: f64 = 4.2;
const DOT_RING_THICK_END: f64 = 1.0;
const DOT_RING_SOFT_MIN: f64 = 0.6;
const DOT_RING_SOFT_MAX: f64 = 1.5;
const DOT_HALO_SOFT: f64 = 0.6;
const DOT_HALO_REVEAL: f64 = 0.16;
const DOT_HALO_ALPHA_MAX: f64 = 0.50;
const RIPPLE_CORE_BACK_MS: u64 = RIPPLE_CYCLE_MS;

/// 绿点环在整个周期里的最大外沿半径（像素）。
///
/// 环是空心的：`ring_r` 是内缘，向外还要占 `壁厚 + 软边`。帧表里这两个量随相位变化，
/// 但总和的上界出现在周期末尾（内缘最大 + 壁厚最小 + 软边最大）：
/// `0.85 × slot + 1.0 + 1.5`。旧帧表按"起始壁厚 4.2 + 软边 1.5"留了 46 px 画布，
/// 多出来的透明边会让 cell 无谓变宽；现在按真实外沿取整。
fn dot_reach(slot: i32) -> f64 {
    DOT_RING_END * f64::from(slot) + DOT_RING_THICK_END + DOT_RING_SOFT_MAX
}

/// 帧表边长：环外沿直径向上取整（偶数），且不小于槽高 + 8（芯与晕总得装得下）。
fn dot_plot_size(slot: i32) -> i32 {
    ((dot_reach(slot).ceil() as i32) * 2).max(slot + 8)
}

/// 绿点独占 cell 的边长：环外沿直径 + 1 px 余量，保证动画永不越出 cell。
fn dot_cell_size(slot: i32) -> i32 {
    dot_plot_size(slot) + 1
}

/// 一行 marks 的列宽：按 cell 顺序累加，两个 cell 之间加一个 [`MARK_GAP`]。
fn mark_column_width(mark: Mark, dot_cell: i32, glyph_cell: i32) -> i32 {
    let mut width = 0;
    if mark.has_dot() {
        width += dot_cell;
    }
    if mark.has_glyph() {
        width += glyph_cell;
        if mark.has_dot() {
            width += MARK_GAP;
        }
    }
    width
}

/// 按绘制游标从左到右排出一行的 cell 矩形：点 cell 在前，符号 cell 在后。
///
/// 所有落位都从这里推出（绘制、动画、不变量测试共用），没有任何 mark 再按
/// "列中心 / 列右缘"定位。
fn mark_cells(
    mark: Mark,
    row_top: i32,
    row_bottom: i32,
    dot_cell: i32,
    glyph_cell: i32,
) -> (Option<RECT>, Option<RECT>) {
    let center = (row_top + row_bottom) / 2;
    let mut left = MENU_FRAME + MENU_PAD_L;
    let dot = if mark.has_dot() {
        let rect = RECT {
            left,
            top: center - dot_cell / 2,
            right: left + dot_cell,
            bottom: center - dot_cell / 2 + dot_cell,
        };
        left += dot_cell;
        Some(rect)
    } else {
        None
    };
    if mark.has_dot() && mark.has_glyph() {
        left += MARK_GAP;
    }
    let glyph = if mark.has_glyph() {
        Some(RECT {
            left,
            top: center - glyph_cell / 2,
            right: left + glyph_cell,
            bottom: center - glyph_cell / 2 + glyph_cell,
        })
    } else {
        None
    };
    (dot, glyph)
}

const SUBSAMPLES: i32 = 4;
const SUB_STEP: f64 = 1.0 / SUBSAMPLES as f64;
const SUB_COUNT: f64 = (SUBSAMPLES * SUBSAMPLES) as f64;

const CLASS_NAME: PCWSTR = w!("LiteSoundMenuWindow");

/// 菜单选择结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// ESC / 点外部 / 没选中
    Cancelled,
    /// 选中第几台设备（`entries` 下标）
    Picked(usize),
    /// "退出"
    Exit,
}

thread_local! {
    /// 同一时刻只有一张菜单：状态整块放线程局部，窗口过程直接取。
    static STATE: RefCell<Option<MenuState>> = const { RefCell::new(None) };
    static CLASS_DONE: Cell<bool> = const { Cell::new(false) };
    /// 上一帧的 tick，仅用于 `LITESOUND_MENU_TRACE` 诊断时算帧间隔。
    static LAST_TICK: Cell<u64> = const { Cell::new(0) };
    /// DirectWrite 文字路径不可用时的降级日志只写一条。
    static DW_FALLBACK_LOGGED: Cell<bool> = const { Cell::new(false) };
    /// 菜单可见期间的低级鼠标钩子（0 = 未装）。
    static MOUSE_HOOK: Cell<isize> = const { Cell::new(0) };
    /// 钩子判断"点在菜单外"用的矩形，装钩子时快照，回调里不再查窗口。
    static HOOK_RECT: Cell<RECT> = const {
        Cell::new(RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        })
    };
    static HOOK_HWND: Cell<isize> = const { Cell::new(0) };
    /// 钩子回调投递过几次关闭请求，仅用于 `LITESOUND_MENU_TRACE` 证明"确实活着又确实卸了"。
    static HOOK_FIRES: Cell<u32> = const { Cell::new(0) };
}

/// 只在菜单可见期间安装的 `WH_MOUSE_LL`。
///
/// 前台锁生效时 `SetForegroundWindow` 可能失败，菜单拿不到焦点，点外部就收不到
/// `WM_ACTIVATE(WA_INACTIVE)` / `WM_KILLFOCUS`，菜单会一直挂着（日志里真出现过一次）。
/// 回调只做三件事：判断坐标在不在菜单矩形内、不在就投一条 `WM_MENU_DONE`、立刻
/// `CallNextHookEx` 往下传。不分配内存、不写日志、不做任何可能阻塞的事。
fn install_mouse_hook(hwnd: HWND) {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rect);
    }
    HOOK_RECT.with(|cell| cell.set(rect));
    HOOK_HWND.with(|cell| cell.set(hwnd.0 as isize));
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }.ok();
    let hook = unsafe {
        SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_hook),
            module.map(|module| HINSTANCE(module.0)),
            0,
        )
    };
    match hook {
        Ok(hook) => {
            MOUSE_HOOK.with(|cell| cell.set(hook.0 as isize));
            if trace_on() {
                diag::log("菜单鼠标钩子已装");
            }
        }
        Err(e) => diag::log(&format!(
            "菜单鼠标钩子安装失败，点外部关闭退回焦点路径: {e}"
        )),
    }
}

fn remove_mouse_hook() {
    let raw = MOUSE_HOOK.with(|cell| cell.replace(0));
    if raw != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(raw as *mut std::ffi::c_void));
        }
        if trace_on() {
            let fires = HOOK_FIRES.with(|cell| cell.replace(0));
            diag::log(&format!("菜单鼠标钩子已卸，本次投递 {fires} 次"));
        }
    }
}

/// 钩子回调里唯一需要判断的东西：点是不是落在菜单矩形内。
///
/// 边界取左闭右开，和 `hit_test` 的窗口判定保持一致。抽成纯函数一是让回调里只剩比较，
/// 二是能被单元测试覆盖（钩子本身要真机点击才触发）。
fn point_in_menu(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let button = wparam.0 as u32;
        if button == WM_LBUTTONDOWN || button == WM_RBUTTONDOWN || button == WM_MBUTTONDOWN {
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            let rect = HOOK_RECT.with(|cell| cell.get());
            if !point_in_menu(&rect, info.pt.x, info.pt.y) {
                let raw = HOOK_HWND.with(|cell| cell.get());
                if raw != 0 {
                    HOOK_FIRES.with(|cell| cell.set(cell.get().saturating_add(1)));
                    unsafe {
                        let _ = PostMessageW(
                            Some(HWND(raw as *mut std::ffi::c_void)),
                            WM_MENU_DONE,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 逐帧计时诊断开关。默认关闭；`LITESOUND_MENU_TRACE=1` 时每帧写一条 diag。
fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LITESOUND_MENU_TRACE").is_some())
}

/// 一张 32 位 BGRA 顶底位图 + 内存 DC。
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut u8,
}

impl Surface {
    fn new(width: i32, height: i32) -> Option<Surface> {
        if width <= 0 || height <= 0 {
            return None;
        }
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // 负高度 = 自顶向下，和 `coverage_map`/合成循环的行序一致
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap =
            match unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) } {
                Ok(bitmap) => bitmap,
                Err(_) => {
                    unsafe {
                        let _ = DeleteDC(dc);
                    }
                    return None;
                }
            };
        if bits.is_null() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(dc);
            }
            return None;
        }
        let previous = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
        if previous.0.is_null() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(dc);
            }
            return None;
        }
        Some(Surface {
            dc,
            bitmap,
            previous,
            pixels: bits.cast(),
        })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

/// 当前行黑条的逐像素覆盖率：内缩 [`MENU_FRAME`] 的圆角矩形，`4×4` 超采样。
///
/// 只覆盖该行的上下界与整窗宽度（黑条本身只画在 `MENU_FRAME` 以内），行外不是它的职责。
/// 菜单窗口只建一次，黑条位置在菜单生命周期里不变，所以这张表在 `show` 时烘一次、每帧只查表。
struct BandMask {
    top: i32,
    height: i32,
    mask: Vec<u8>,
}

impl BandMask {
    /// `row_top` / `row_bottom` 是**行**的上下沿；黑条自己还要再内缩：
    /// 左右 [`BAND_INSET_X`]（比原来的 3 px 再刻意多留 6 px，留出下陷的"边"）、
    /// 上下 [`BAND_INSET_Y`]，四角圆角 [`BAND_RADIUS`]。
    fn bake(width: i32, row_top: i32, row_bottom: i32) -> Option<BandMask> {
        let top = row_top + BAND_INSET_Y;
        let bottom = row_bottom - BAND_INSET_Y;
        let height = bottom - top;
        let left = BAND_INSET_X;
        let right = width - BAND_INSET_X;
        let radius = f64::from(BAND_RADIUS);
        if height <= 0 || right <= left {
            return None;
        }
        let mut mask = vec![0u8; (width * height) as usize];
        for y in 0..height {
            for x in left..right {
                let mut hits = 0u32;
                for sy in 0..SUBSAMPLES {
                    for sx in 0..SUBSAMPLES {
                        let px = f64::from(x) + (f64::from(sx) + 0.5) * SUB_STEP;
                        let py = f64::from(top + y) + (f64::from(sy) + 0.5) * SUB_STEP;
                        if inside_rounded_rect(
                            px,
                            py,
                            f64::from(left),
                            f64::from(top),
                            f64::from(right),
                            f64::from(bottom),
                            radius,
                        ) {
                            hits += 1;
                        }
                    }
                }
                mask[(y * width + x) as usize] =
                    ((f64::from(hits) / SUB_COUNT) * 255.0).round() as u8;
            }
        }
        Some(BandMask { top, height, mask })
    }

    fn at(&self, x: i32, y: i32, width: i32) -> u8 {
        let row = y - self.top;
        if row < 0 || row >= self.height {
            return 0;
        }
        self.mask[(row * width + x) as usize]
    }

    /// 内阴影的逐行色调偏移（正 = 抬亮，负 = 压暗），按**设备像素行**给值。
    ///
    /// **上下镜像**：到最近一条横壁的行距 `edge = min(row, height-1-row)` 唯一决定色调，
    /// 所以 `tone(top+i) == tone(bottom-1-i)` 是构造保证的，不靠两段代码各自算对。
    /// 壁长 [`BAND_WALL_PX`]，壁沿压到 `wall`，缓升到槽底 `floor` 后恒为 `floor`。
    ///
    /// 深色：`floor = 12`、`wall = 0`。旧版是"顶部 0→8 + 底缘两行 `0x1C`/`0x3E`（28/62）"，
    /// 末行 62 比菜单底 `0x2C`（44）还亮——纯黑槽下方就多出一条独立的灰线，而且上下方向
    /// 相反，观感上两边不统一（2026-11-04 真机反馈）。现在两侧都压暗、峰值 0，槽底 12 比
    /// 菜单底深 32，凹陷靠这条 8 px 的深壁读出来，不再有亮线。
    ///
    /// 浅色：`floor = -8`（浅凹槽）、`wall = -16`（更深的内壁），同样 8 px、上下镜像；
    /// 旧版底缘的 +6/+14 受光一并去掉，两套主题的阴影方向一致。
    fn tone(&self, y: i32, dark: bool) -> i16 {
        let row = y - self.top;
        if row < 0 || row >= self.height {
            return 0;
        }
        let (floor, wall) = if dark { (12i16, 0i16) } else { (-8i16, -16i16) };
        let ramp = BAND_WALL_PX.min(self.height / 2).max(1);
        let edge = row.min(self.height - 1 - row);
        if edge >= ramp {
            return floor;
        }
        // 二次缓升：贴近壁沿的变化更细，向内收得干净（`edge/ramp` 的平方）。
        let t = f64::from(edge) / f64::from(ramp);
        wall + (f64::from(floor - wall) * t * t).round() as i16
    }
}

/// 内缩圆角矩形：四个角按 `radius` 削圆，其余区域满覆盖。
fn inside_rounded_rect(
    px: f64,
    py: f64,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    radius: f64,
) -> bool {
    if px < left || py < top || px >= right || py >= bottom {
        return false;
    }
    let cx = px.clamp(left + radius, (right - radius).max(left + radius));
    let cy = py.clamp(top + radius, (bottom - radius).max(top + radius));
    let dx = px - cx;
    let dy = py - cy;
    dx * dx + dy * dy <= radius * radius
}

/// 一行菜单。`kind` 同时决定能不能点、点了返回什么。
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Device(usize),
    Separator,
    Exit,
}

struct MenuRow {
    top: i32,
    bottom: i32,
    kind: RowKind,
    text: Vec<u16>,
    /// 静态标记（非当前行用）。当前行的绿点由动画帧取代，`mark` 只留蓝牙符号。
    mark: Mark,
    dim: bool,
}

struct Layout {
    width: i32,
    height: i32,
    rows: Vec<MenuRow>,
    current: Option<usize>,
    marker_width: i32,
    marker_height: i32,
    dot_center: Option<(i32, i32)>,
    dark: bool,
    palette: Palette,
}

impl Layout {
    fn build(entries: &[Entry], default: Option<&str>) -> Option<Layout> {
        let (font, px) = menu_font();
        if px <= 0 {
            return None;
        }
        let (_, marker_height) = mark_geometry()?;
        let row_height = menu_row_height(px);
        let current = entries
            .iter()
            .position(|entry| crate::history::same_id(default, Some(entry.id.as_str())));

        let mut rows: Vec<MenuRow> = Vec::with_capacity(entries.len() + 2);
        let mut widest_text = 0;
        let mut y = MENU_FRAME;
        for (index, entry) in entries.iter().enumerate() {
            let text = wide(&naming::menu_text(&entry.name));
            widest_text = widest_text.max(text_width(&text, font));
            let is_current = current == Some(index);
            // marks 是有序集合：当前那台有点，蓝牙那台有符号，两者都成立就是 [Dot, Glyph]。
            let mark = match (is_current, entry.bluetooth) {
                (true, true) => Mark::Both,
                (true, false) => Mark::Dot,
                (false, true) => Mark::Bluetooth,
                (false, false) => Mark::Blank,
            };
            rows.push(MenuRow {
                top: y,
                bottom: y + row_height,
                kind: RowKind::Device(index),
                text,
                mark,
                dim: !is_current,
            });
            y += row_height;
        }

        rows.push(MenuRow {
            top: y,
            bottom: y + MENU_SEPARATOR_PX,
            kind: RowKind::Separator,
            text: Vec::new(),
            mark: Mark::Blank,
            dim: false,
        });
        y += MENU_SEPARATOR_PX;

        let exit_text = wide("退出");
        widest_text = widest_text.max(text_width(&exit_text, font));
        rows.push(MenuRow {
            top: y,
            bottom: y + EXIT_ROW_PX,
            kind: RowKind::Exit,
            text: exit_text,
            mark: Mark::Blank,
            dim: false,
        });
        y += EXIT_ROW_PX;

        // 菜单宽度取所有行的最大列宽：各行文字因此从同一个 x 起排。
        let dot_cell = dot_cell_size(marker_height);
        let marker_width = rows
            .iter()
            .map(|row| mark_column_width(row.mark, dot_cell, marker_height))
            .max()
            .unwrap_or(0);
        let width = item_width(widest_text, marker_width);

        // 点永远在一行的第一个 cell：圆心 = padding + 点 cell 中心。
        let dot_center = current.map(|index| {
            let row = &rows[index];
            (
                MENU_FRAME + MENU_PAD_L + dot_cell / 2,
                (row.top + row.bottom) / 2,
            )
        });

        Some(Layout {
            width: width.max(1) + MENU_FRAME * 2,
            height: y + MENU_FRAME,
            rows,
            current,
            marker_width,
            marker_height,
            dot_center,
            dark: menu_is_dark(),
            palette: menu_palette(menu_is_dark()),
        })
    }
}

/// 一条条目的宽度：左内边距 + 标记列 + 间距 + 文字 + 右内边距。
///
/// 所有行共用同一份 `marker_width`（全场最大列宽），所以这里收的是**文字像素宽**，
/// 不再每行自己量一遍。
fn item_width(text_width: i32, marker_width: i32) -> i32 {
    MENU_PAD_L + marker_width + MENU_GAP + text_width + MENU_PAD_R
}

/// 预烘的绿点帧表：`RIPPLE_FRAMES` 张 `size × size` 的**每像素 1 字节覆盖率**，帧连续排列。
///
/// 整个绿点只有一种颜色（`DOT_RGB`），旧版每像素存 4 字节预乘 BGRA 里的三个颜色通道
/// 全是 `dot[c] * alpha / 255` 的重复结果。只留覆盖率、着色放到合成时做，单帧表从
/// 4·size²·帧数 降到 size²·帧数（46×46×51 从 529 KB 降到 106 KB）。
///
/// 着色公式与旧版逐字节相同（先 `dot*alpha/255` 取整再叠底），所以像素结果不是"接近"，
/// 是同一份值（见 `dot_blend_matches_premultiplied` 单测）。
struct DotPlot {
    size: i32,
    left: i32,
    top: i32,
    /// 长度 = `size * size * RIPPLE_FRAMES`，第 `frame` 帧从 `frame * size * size` 开始。
    coverage: Vec<u8>,
}

struct MenuState {
    hwnd: HWND,
    x: i32,
    y: i32,
    layout: Layout,
    /// 唯一的像素面：GDI 先把**原始**颜色画进来，合成再把这一块就地换成预乘结果。
    ///
    /// 旧版是 scratch（原始色）+ layer（预乘）两张 DIB；scratch 的内容只在"画完到合成"
    /// 之间有意义，静态行合成过一次就再也不读了，所以可以合到一张。代价是当前行动起来
    /// 的那一行每帧都要重画原始像素（`render_frame` 里先 `paint_row` 再 `compose`）。
    layer: Surface,
    coverage: Vec<u8>,
    band: Option<BandMask>,
    dot: Option<Rc<DotPlot>>,
    hover: Option<usize>,
    down: Option<usize>,
    tracking: bool,
    /// 整窗静态内容是否已经画进 `layer`。画过一次之后每帧只重画悬停变化的行。
    painted: bool,
    /// 悬停变化导致 `layer` 需要重画的行下标（每帧清空）。
    dirty: Vec<usize>,
    result: Outcome,
    done: bool,
}

/// 最近一次烘焙的绿点帧表。单份上限：键（标记槽 + 圆心）变了就整份替换，
/// 过期（`DOT_CACHE_MAX_MS`）或菜单正常打开过一段时间则在 [`settle_dot_cache`] 里 drop。
struct DotCache {
    slot: i32,
    center: (i32, i32),
    baked_ms: u64,
    plot: Rc<DotPlot>,
}

thread_local! {
    static DOT_CACHE: RefCell<Option<DotCache>> = const { RefCell::new(None) };
}

/// 取绿点帧表：命中缓存就复用，否则烘一份并放进缓存（覆盖旧的，缓存容量恒为 1）。
fn obtain_dot(slot: i32, center: (i32, i32)) -> Option<Rc<DotPlot>> {
    if slot <= 0 {
        return None;
    }
    let now = unsafe { GetTickCount64() };
    let reused = DOT_CACHE.with(|cell| {
        let mut held = cell.borrow_mut();
        match held.as_ref() {
            Some(cache)
                if cache.slot == slot
                    && cache.center == center
                    && now.saturating_sub(cache.baked_ms) <= DOT_CACHE_MAX_MS =>
            {
                Some(cache.plot.clone())
            }
            Some(_) => {
                *held = None;
                None
            }
            None => None,
        }
    });
    if let Some(plot) = reused {
        return Some(plot);
    }

    let plot = Rc::new(bake_dot(slot, center)?);
    DOT_CACHE.with(|cell| {
        *cell.borrow_mut() = Some(DotCache {
            slot,
            center,
            baked_ms: now,
            plot: plot.clone(),
        });
    });
    Some(plot)
}

/// 菜单收起后决定帧表去留：连续开关（菜单活得很短）留着复用，正常使用就 drop，
/// 空闲私有内存回到基线。过期或键变化的那份在 [`obtain_dot`] 里已经换掉。
fn settle_dot_cache(opened_ms: u64) {
    let now = unsafe { GetTickCount64() };
    let rapid = now.saturating_sub(opened_ms) < DOT_CACHE_RAPID_MS;
    let kept = rapid
        && DOT_CACHE.with(|cell| {
            cell.borrow()
                .as_ref()
                .is_some_and(|cache| now.saturating_sub(cache.baked_ms) <= DOT_CACHE_MAX_MS)
        });
    if !kept {
        DOT_CACHE.with(|cell| *cell.borrow_mut() = None);
    }
    if trace_on() {
        diag::log(&format!(
            "菜单帧表{}",
            if kept {
                "保留（连续开关）"
            } else {
                "已释放"
            }
        ));
    }
}

#[cfg(test)]
fn reset_dot_cache() {
    DOT_CACHE.with(|cell| *cell.borrow_mut() = None);
}

#[cfg(test)]
fn dot_cache_entries() -> usize {
    DOT_CACHE.with(|cell| usize::from(cell.borrow().is_some()))
}

/// 显示菜单并跑它自己的模态消息循环，返回用户的选择。
pub fn show(entries: &[Entry], default: Option<&str>) -> Outcome {
    let opened_ms = unsafe { GetTickCount64() };
    let Some(layout) = Layout::build(entries, default) else {
        diag::log("菜单布局构建失败，本次不弹菜单");
        return Outcome::Cancelled;
    };
    let (width, height) = (layout.width, layout.height);
    if trace_on() {
        diag::log(&format!("菜单布局 {width}x{height}"));
    }

    let mut cursor = POINT::default();
    let _ = unsafe { GetCursorPos(&mut cursor) };
    let (x, y) = place_menu(cursor.x, cursor.y, width, height);

    let Some(layer) = Surface::new(width, height) else {
        diag::log("菜单位图创建失败，本次不弹菜单");
        return Outcome::Cancelled;
    };
    let coverage = coverage_map(width, height);
    let band = layout.current.and_then(|index| {
        let row = &layout.rows[index];
        BandMask::bake(width, row.top, row.bottom)
    });
    let dot = layout
        .dot_center
        .and_then(|center| obtain_dot(layout.marker_height, center));

    let mut state = MenuState {
        hwnd: HWND::default(),
        x,
        y,
        layout,
        layer,
        coverage,
        band,
        dot,
        hover: None,
        down: None,
        tracking: false,
        painted: false,
        dirty: Vec::new(),
        result: Outcome::Cancelled,
        done: false,
    };
    state.hover = hit_test(&state.layout.rows, cursor.x - x, cursor.y - y);
    STATE.with(|slot| *slot.borrow_mut() = Some(state));

    if !ensure_class() {
        diag::log("菜单窗口类注册失败，本次不弹菜单");
        STATE.with(|slot| *slot.borrow_mut() = None);
        return Outcome::Cancelled;
    }
    let instance = match unsafe { GetModuleHandleW(PCWSTR::null()) } {
        Ok(module) => HINSTANCE(module.0),
        Err(e) => {
            diag::log(&format!("菜单取模块句柄失败: {e}"));
            STATE.with(|slot| *slot.borrow_mut() = None);
            return Outcome::Cancelled;
        }
    };
    let ex_style = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
    let hwnd = match unsafe {
        CreateWindowExW(
            ex_style,
            CLASS_NAME,
            w!(""),
            WS_POPUP,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance),
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            diag::log(&format!("菜单窗口创建失败: {e}"));
            STATE.with(|slot| *slot.borrow_mut() = None);
            return Outcome::Cancelled;
        }
    };
    STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.hwnd = hwnd;
        }
    });

    // Win11 默认会再给顶层窗口裁一次圆角：那是**硬裁形**，会直接削掉自绘 alpha 外圈的几档
    // 半透明（下沿角部就变成"突然截断"）。圆角形状在 [`coverage_map`] 里已经完整负责，
    // 这里显式关掉系统这层裁剪。Win10 没有这个属性，失败也不影响任何行为。
    let preference = DWMWCP_DONOTROUND;
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    };

    // 先在隐藏状态下提交首帧，再显示：屏上第一次出现就是完整画面，没有空白/半张的过渡帧。
    render_frame(hwnd);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST),
            x,
            y,
            width,
            height,
            SWP_SHOWWINDOW | SWP_NOACTIVATE,
        );
    }
    focus_menu(hwnd);
    install_mouse_hook(hwnd);

    // 帧节拍：高精度可等待定时器 + `MsgWaitForMultipleObjectsEx`。
    //
    // `SetTimer` 的实测中位延迟 47 ms、且漏拍不补，33 ms 的动效会被拉成 20 fps。可等待
    // 定时器与消息等待同一次调用，到点立刻返回；相位仍从 `GetTickCount64` 取，所以即使
    // 某一拍迟了，下一帧也直接跳到**当前时间**对应的时间索引，不会累积漂移。
    let timer = frame_timer();
    let mut msg = MSG::default();
    'menu: loop {
        // 先把队列排空（`PeekMessageW` 也会派发 sent 消息，`WM_ACTIVATE` 那条路照旧生效）。
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                // 托盘退出：把 WM_QUIT 还回队列，外层消息循环照常收尾
                unsafe { PostQuitMessage(0) };
                break 'menu;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if menu_done() {
                break 'menu;
            }
        }
        if menu_done() {
            break;
        }

        let wait = unsafe {
            MsgWaitForMultipleObjectsEx(
                timer.as_ref().map(std::slice::from_ref),
                if timer.is_some() { INFINITE } else { FRAME_MS },
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        if wait == WAIT_FAILED {
            diag::log("菜单消息等待失败，提前收起");
            break;
        }
        // 定时器到点，或者没有高精度定时器时超时，都画下一帧；输入到达则回到上面排空。
        if wait == WAIT_OBJECT_0 || (timer.is_none() && wait == WAIT_TIMEOUT) {
            render_frame(hwnd);
        }
    }

    remove_mouse_hook();
    if let Some(handle) = timer {
        unsafe {
            let _ = CloseHandle(handle);
        }
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    let taken = STATE
        .with(|slot| slot.borrow_mut().take())
        .map_or(Outcome::Cancelled, |state| state.result);
    // 菜单状态已经 drop（含 `scratch`/`layer`）；帧表按本次菜单活得长短决定留还是扔。
    settle_dot_cache(opened_ms);
    taken
}

fn ensure_class() -> bool {
    if CLASS_DONE.with(Cell::get) {
        return true;
    }
    let Ok(module) = (unsafe { GetModuleHandleW(PCWSTR::null()) }) else {
        return false;
    };
    let class = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: HINSTANCE(module.0),
        hIcon: HICON::default(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: CLASS_NAME,
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return false;
    }
    CLASS_DONE.with(|done| done.set(true));
    true
}

/// 落位：从光标往右下展开；放不下就翻到上方/左方，最后夹进工作区。
///
/// 托盘在右下角，这条规则让菜单贴着托盘出现，而不是被系统摆到屏幕中央。
fn place_menu(cursor_x: i32, cursor_y: i32, width: i32, height: i32) -> (i32, i32) {
    let point = POINT {
        x: cursor_x,
        y: cursor_y,
    };
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        info.rcWork = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1600,
        };
    }
    let work = info.rcWork;
    menu_origin(point, width, height, work)
}

/// [`place_menu`] 的纯几何部分（单测直接钉它）。
fn menu_origin(cursor: POINT, width: i32, height: i32, work: RECT) -> (i32, i32) {
    let mut x = cursor.x;
    let mut y = cursor.y;
    if x + width > work.right {
        x = work.right - width;
    }
    if y + height > work.bottom {
        y = work.bottom - height;
    }
    x = x.max(work.left);
    y = y.max(work.top);
    (x, y)
}

/// 把菜单做成前台窗口——**ESC / 点外关闭全靠这一步**。
///
/// 常规路径（用户真的点了托盘图标）一次 `SetForegroundWindow` 就成。但 Windows 的前台锁
/// 在"上一个前台窗口刚消失"之类的时机会短暂拒绝这个调用，菜单于是拿不到键盘焦点，
/// ESC 与 `WM_ACTIVATE(WA_INACTIVE)` 都不会来，只能靠点条目收场。真机复现里确实遇到过。
/// 失败时把本线程的输入队列临时挂到当前前台线程上再设一次前台与焦点，这条路不受前台锁
/// 限制；两次都不成只记日志，画面照常显示。
fn focus_menu(hwnd: HWND) {
    let foreground_is_menu = || unsafe { GetForegroundWindow() == hwnd };
    if unsafe { SetForegroundWindow(hwnd) }.as_bool() && foreground_is_menu() {
        return;
    }

    let foreground = unsafe { GetForegroundWindow() };
    let foreground_thread = unsafe { GetWindowThreadProcessId(foreground, None) };
    let our_thread = unsafe { GetCurrentThreadId() };
    unsafe {
        let _ = AttachThreadInput(our_thread, foreground_thread, true);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        let _ = AttachThreadInput(our_thread, foreground_thread, false);
    }

    if !foreground_is_menu() {
        diag::log("菜单未能取得前台，ESC / 点外关闭本次可能失效");
    }
}

/// 建 33 ms 周期的高精度可等待定时器；建不出来就返回 `None`，循环退回 33 ms 超时等待。
///
/// 先试 `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`（Win10 1803+），失败再退一次不带标志的
/// 普通定时器：老系统至少还能拿到毫秒级节拍，不至于退回 `SetTimer` 那条 47 ms 的路。
fn frame_timer() -> Option<HANDLE> {
    let timer = unsafe {
        CreateWaitableTimerExW(
            None,
            PCWSTR::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )
        .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
    };
    let timer = match timer {
        Ok(timer) => timer,
        Err(e) => {
            diag::log(&format!("菜单高精度定时器创建失败，退回超时等待: {e}"));
            return None;
        }
    };
    // 相对时间以 100 ns 为单位；周期 33 ms，让定时器自己循环，不用每帧重置。
    let due = -(FRAME_MS as i64 * 10_000);
    if let Err(e) = unsafe { SetWaitableTimer(timer, &due, FRAME_MS as i32, None, None, false) } {
        diag::log(&format!("菜单定时器启动失败，退回超时等待: {e}"));
        unsafe {
            let _ = CloseHandle(timer);
        }
        return None;
    }
    Some(timer)
}

fn menu_done() -> bool {
    STATE.with(|slot| slot.borrow().as_ref().is_none_or(|state| state.done))
}

/// 改悬停行并登记需要重画的两行（旧行要擦、新行要亮）。
fn set_hover(state: &mut MenuState, next: Option<usize>) -> bool {
    if state.hover == next {
        return false;
    }
    if let Some(old) = state.hover {
        state.dirty.push(old);
    }
    if let Some(new) = next {
        state.dirty.push(new);
    }
    state.hover = next;
    true
}

fn render_frame(hwnd: HWND) {
    let started = std::time::Instant::now();
    let tick = unsafe { GetTickCount64() };
    let gap = LAST_TICK.with(|cell| {
        let previous = cell.get();
        cell.set(tick);
        tick.saturating_sub(previous)
    });
    STATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        // 首帧完整画一遍；之后只重画悬停变化的行。当前行（扫光 + 黑条 + 绿点）每帧都要
        // 先画回原始像素再合成：合并成一张 DIB 后，上一帧写入的已经是预乘后的最终值，
        // 直接再叠扫光会越叠越黑。
        let mut rects = [RECT::default(); 4];
        let mut count = 0usize;
        if state.painted {
            while let Some(index) = state.dirty.pop() {
                paint_row(state, index);
                // 当前行的矩形由下面的动画矩形统一合成，避免同一块像素叠两遍扫光。
                if state.layout.current == Some(index) {
                    continue;
                }
                rects[count] = row_rect(state, index);
                count += 1;
            }
        } else {
            paint_rows(state);
            rects[0] = clip_rect(
                state,
                RECT {
                    left: 0,
                    top: 0,
                    right: state.layout.width,
                    bottom: state.layout.height,
                },
            );
            count = 1;
            state.painted = true;
        }
        if let Some(index) = state.layout.current {
            paint_row(state, index);
            if let Some(rect) = animation_rect(state) {
                if count < rects.len() {
                    rects[count] = rect;
                    count += 1;
                }
            }
        }
        compose(state, &rects[..count]);
        let cpu = started.elapsed().as_micros();
        update_layered(hwnd, state);
        if trace_on() {
            let submit = started.elapsed().as_micros() - cpu;
            diag::log(&format!(
                "菜单帧 gap={gap} ms cpu={cpu} µs 提交={submit} µs 矩形={count}"
            ));
        }
    });
    // 每帧重申：快速设置/任务栏浮出窗也在 TOPMOST 带，谁后建谁在上并不可控。
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 全量重画整窗原始色：先铺底色，再逐行画。
///
/// 只在**首帧**调用：菜单窗口整个生命周期里布局与静态内容不变，之后的行重画由
/// [`paint_row`] 按需触发（悬停变化、当前行每帧的重画）。画进来的都是**原始色**，
/// 必须紧跟一次 [`compose`] 才能变成预乘后的可提交像素。
fn paint_rows(state: &MenuState) {
    let (width, height) = (state.layout.width, state.layout.height);
    fill_rect(
        state.layer.dc,
        &RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        state.layout.palette.bg,
    );
    for index in 0..state.layout.rows.len() {
        paint_row(state, index);
    }
}

/// 重画一行：底色 + 分隔线 / 标记 + 文字。
///
/// 画满 `MENU_FRAME..width-MENU_FRAME` 的整行矩形，所以调用方不必先擦旧像素。
fn paint_row(state: &MenuState, index: usize) {
    let (width, _) = (state.layout.width, state.layout.height);
    let dc = state.layer.dc;
    let palette = state.layout.palette;
    let row = &state.layout.rows[index];
    let is_current = state.layout.current == Some(index);
    // 当前行永远按底色画，再被扫光压成黑条；悬停高亮只作用于其它行。
    let background = if state.hover == Some(index) && !is_current {
        palette.hover
    } else {
        palette.bg
    };
    let rect = RECT {
        left: MENU_FRAME,
        top: row.top,
        right: width - MENU_FRAME,
        bottom: row.bottom,
    };
    fill_rect(dc, &rect, background);

    if row.kind == RowKind::Separator {
        let y = (row.top + row.bottom) / 2;
        fill_rect(
            dc,
            &RECT {
                left: MENU_FRAME + MENU_PAD_L,
                top: y,
                right: width - MENU_FRAME - MENU_PAD_R,
                bottom: y + 1,
            },
            palette.separator,
        );
        return;
    }

    // 标记按 cell 顺序画：点由合成阶段叠动画帧，这里只贴蓝牙符号。
    let dot_cell = dot_cell_size(state.layout.marker_height);
    let glyph_cell = state.layout.marker_height;
    let (_, glyph_rect) = mark_cells(row.mark, row.top, row.bottom, dot_cell, glyph_cell);
    if let Some(cell) = glyph_rect {
        if let Some(bitmap) = glyph_bitmap(glyph_cell, glyph_cell) {
            draw_mark(
                dc,
                cell.left,
                cell.top,
                cell.right - cell.left,
                cell.bottom - cell.top,
                bitmap,
            );
        }
    }
    let (font, _) = menu_font();
    draw_text(
        dc,
        &rect,
        row,
        font,
        background,
        palette,
        state.layout.marker_width,
    );
}

fn row_rect(state: &MenuState, index: usize) -> RECT {
    let row = &state.layout.rows[index];
    RECT {
        left: MENU_FRAME,
        top: row.top,
        right: state.layout.width - MENU_FRAME,
        bottom: row.bottom,
    }
}

/// 每帧必须变的最小矩形：当前行整条（扫光 + 黑条下陷）加上绿点的外接框。
fn animation_rect(state: &MenuState) -> Option<RECT> {
    let index = state.layout.current?;
    let row = &state.layout.rows[index];
    let mut rect = RECT {
        left: MENU_FRAME,
        top: row.top,
        right: state.layout.width - MENU_FRAME,
        bottom: row.bottom,
    };
    if let Some(plot) = state.dot.as_ref() {
        rect.left = rect.left.min(plot.left);
        rect.top = rect.top.min(plot.top);
        rect.right = rect.right.max(plot.left + plot.size);
        rect.bottom = rect.bottom.max(plot.top + plot.size);
    }
    Some(clip_rect(state, rect))
}

fn clip_rect(state: &MenuState, rect: RECT) -> RECT {
    RECT {
        left: rect.left.max(0),
        top: rect.top.max(0),
        right: rect.right.min(state.layout.width),
        bottom: rect.bottom.min(state.layout.height),
    }
}

fn draw_text(
    dc: HDC,
    rect: &RECT,
    row: &MenuRow,
    font: HFONT,
    background: u32,
    palette: Palette,
    marker_width: i32,
) {
    let draw = RECT {
        left: rect.left + MENU_PAD_L + marker_width + MENU_GAP,
        top: rect.top - MENU_TEXT_RISE,
        right: rect.right - MENU_PAD_R,
        bottom: rect.bottom - MENU_TEXT_RISE,
    };
    let foreground = if row.dim { palette.dim } else { palette.text };

    // DirectWrite 覆盖率路径优先：最低一档的灰度 AA，分层窗下没有 ClearType 彩边。
    // 失败（初始化失败 / 本次绘制失败）必须退回原 GDI 路径，行为与 2026-11-03 之前一致。
    let (_, px) = menu_font();
    let cell_px = menu_cell_height(dc, font);
    if crate::dwtext::draw(dc, &draw, &row.text, px, cell_px, background, foreground) {
        return;
    }
    DW_FALLBACK_LOGGED.with(|logged| {
        if !logged.get() {
            logged.set(true);
            diag::log("DirectWrite 菜单文字路径不可用，本行及后续文字回退 GDI DrawTextW");
        }
    });

    let mut text = row.text.clone();
    let previous = unsafe { SelectObject(dc, HGDIOBJ(font.0)) };
    unsafe {
        // OPAQUE + 同底色：灰度抗锯齿需要知道底色才能把边缘混平（2026-11-02 实测结论）。
        SetBkMode(dc, OPAQUE);
        SetBkColor(dc, COLORREF(background));
        SetTextColor(dc, COLORREF(foreground));
        let mut rect = draw;
        DrawTextW(
            dc,
            &mut text,
            &mut rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        SelectObject(dc, previous);
    }
}

/// 菜单字体的 GDI 单元格高（`tmHeight`），DirectWrite 折算 em 要用。只量一次。
///
/// 不能拿 `lfHeight` 代替：`lfHeight` 是请求值（150% 下 28.5 四舍五入成 29），
/// 实际生效的 `tmHeight` 才是 28，两者差 1 px 就够把行尾那个字顶出右边界。
fn menu_cell_height(dc: HDC, font: HFONT) -> i32 {
    thread_local! {
        static HEIGHT: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    }
    HEIGHT.with(|cell| {
        let known = cell.get();
        if known > 0 {
            return known;
        }
        let mut metrics = TEXTMETRICW::default();
        let previous = unsafe { SelectObject(dc, HGDIOBJ(font.0)) };
        let ok = unsafe { GetTextMetricsW(dc, &mut metrics).as_bool() };
        unsafe {
            SelectObject(dc, previous);
        }
        let height = if ok { metrics.tmHeight } else { 0 };
        cell.set(height);
        height
    })
}

/// 在给定矩形里就地合成：圆角覆盖率乘进 alpha，当前行叠扫光，动画绿点 src-over。
///
/// 读和写都在同一张 `layer` 上（Step 6 合并 DIB）：每个像素先读出原始色、算完再写回，
/// 所以必须保证"重画原始像素"发生在合成之前（见 [`render_frame`] 里当前行的处理）。
fn compose(state: &MenuState, rects: &[RECT]) {
    compose_at(state, rects, unsafe { GetTickCount64() });
}

/// [`compose`] 的可测版本：相位时刻显式传入，离线比对可以逐帧复现同一画面。
fn compose_at(state: &MenuState, rects: &[RECT], now: u64) {
    let width = state.layout.width;
    let src = state.layer.pixels;
    let dst = state.layer.pixels;
    let coverage = &state.coverage;
    let dark = state.layout.dark;
    let background = bgr(state.layout.palette.bg);

    let progress = (now % PERIOD_MS) as f64 / PERIOD_MS as f64;
    let frame = ((now % RIPPLE_TABLE_MS) / FRAME_MS as u64) as usize;

    // 当前行每一列的亮度倍数只算一次；行内 x 从 0 起（相对条目左边）。
    let wave = state.layout.current.map(|index| {
        let row = &state.layout.rows[index];
        let row_width = (width - MENU_FRAME * 2).max(1);
        let gains: Vec<f64> = (0..row_width)
            .map(|x| gain(x, row_width, progress, dark))
            .collect();
        (row.top, row.bottom, gains)
    });

    let dot = state.dot.as_ref();
    let dot_frame = dot.map(|plot| {
        let stride = (plot.size * plot.size) as usize;
        plot.coverage[frame * stride..].as_ptr()
    });
    let dot_rgb = dot_bgr();

    let band_bounds = state.band.as_ref().map(|band| {
        (
            band.top - BAND_INSET_Y,
            band.top + band.height + BAND_INSET_Y,
        )
    });

    for rect in rects {
        for y in rect.top..rect.bottom {
            let in_wave = wave
                .as_ref()
                .map(|(top, bottom, _)| y >= *top && y < *bottom)
                .unwrap_or(false);
            let in_band = band_bounds
                .map(|(top, bottom)| y >= top && y < bottom)
                .unwrap_or(false);
            let tone = if in_band {
                state
                    .band
                    .as_ref()
                    .map_or(0, |band| i32::from(band.tone(y, dark)))
            } else {
                0
            };
            for x in rect.left..rect.right {
                let index = ((y * width + x) * 4) as usize;
                let mut b = unsafe { *src.add(index) };
                let mut g = unsafe { *src.add(index + 1) };
                let mut r = unsafe { *src.add(index + 2) };

                if in_wave && x >= MENU_FRAME && x < width - MENU_FRAME {
                    if let Some((_, _, gains)) = wave.as_ref() {
                        let factor = gains[(x - MENU_FRAME) as usize];
                        let pixel = if dark {
                            ink_on_black(background, [b, g, r], factor)
                        } else {
                            shade(background, [b, g, r], factor)
                        };
                        b = pixel[0];
                        g = pixel[1];
                        r = pixel[2];
                    }
                }

                // 黑条自己的圆角 + 下陷：整行（含四周内缩留边）都按覆盖率混回底色，行外像素原样保留。
                if let Some(band) = state.band.as_ref() {
                    if in_band {
                        let m = u32::from(band.at(x, y, width));
                        // B 方案的下陷色调：深色抬亮、浅色压暗（正负两个方向），
                        // 抬升/压暗放在覆盖率混合之前，圆角过渡才不会被染色。
                        if m != 0 && tone != 0 {
                            let shift = |c: u8| -> u8 {
                                if tone > 0 {
                                    (u32::from(c) + (tone as u32) * (255 - u32::from(c)) / 255)
                                        as u8
                                } else {
                                    (i32::from(c) + tone).clamp(0, 255) as u8
                                }
                            };
                            b = shift(b);
                            g = shift(g);
                            r = shift(r);
                        }
                        if m != 255 {
                            let keep = 255 - m;
                            b = ((u32::from(b) * m + u32::from(background[0]) * keep) / 255) as u8;
                            g = ((u32::from(g) * m + u32::from(background[1]) * keep) / 255) as u8;
                            r = ((u32::from(r) * m + u32::from(background[2]) * keep) / 255) as u8;
                        }
                    }
                }

                if let (Some(plot), Some(frame_pixels)) = (dot, dot_frame) {
                    let dx = x - plot.left;
                    let dy = y - plot.top;
                    if dx >= 0 && dy >= 0 && dx < plot.size && dy < plot.size {
                        let coverage_px =
                            unsafe { *frame_pixels.add((dy * plot.size + dx) as usize) };
                        if coverage_px != 0 {
                            let pixel = blend_dot(dot_rgb, coverage_px, [b, g, r]);
                            b = pixel[0];
                            g = pixel[1];
                            r = pixel[2];
                        }
                    }
                }

                let alpha = coverage[(y * width + x) as usize] as u32;
                unsafe {
                    *dst.add(index) = ((b as u32 * alpha) / 255) as u8;
                    *dst.add(index + 1) = ((g as u32 * alpha) / 255) as u8;
                    *dst.add(index + 2) = ((r as u32 * alpha) / 255) as u8;
                    *dst.add(index + 3) = alpha as u8;
                }
            }
        }
    }
}

fn update_layered(hwnd: HWND, state: &MenuState) {
    let destination = POINT {
        x: state.x,
        y: state.y,
    };
    let source = POINT { x: 0, y: 0 };
    let size = SIZE {
        cx: state.layout.width,
        cy: state.layout.height,
    };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = unsafe {
        UpdateLayeredWindow(
            hwnd,
            None,
            Some(&destination),
            Some(&size),
            Some(state.layer.dc),
            Some(&source),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    };
}

/// 圆角覆盖率：只对四角半径方块做 4×4 超采样，其余像素恒 255。
///
/// **四角只算一次**：左上角象限算出的值按 `y → height-1-y`、`x → width-1-x` 镜像写进另外三个
/// 角，所以上下翻转 + 左右镜像后逐元素相等是由构造保证的，不靠两套代码"碰巧算得一样"。
/// 半径只有一个出处 [`MENU_RADIUS`]（超出半窗时按半窗夹一次，防止四角互相覆盖）。
fn coverage_map(width: i32, height: i32) -> Vec<u8> {
    let mut map = vec![255u8; (width * height) as usize];
    let r = MENU_RADIUS.min(width / 2).min(height / 2);
    if r <= 0 {
        return map;
    }
    let bottom = height - 1;
    for y in 0..r {
        for x in 0..r {
            let value = corner_coverage(x, y, r);
            let top_row = y * width;
            let bottom_row = (bottom - y) * width;
            map[(top_row + x) as usize] = value;
            map[(top_row + width - 1 - x) as usize] = value;
            map[(bottom_row + x) as usize] = value;
            map[(bottom_row + width - 1 - x) as usize] = value;
        }
    }
    map
}

/// 左上角象限内一像素的覆盖率：圆心在 `(r, r)` 的圆弧，`4×4` 超采样。
fn corner_coverage(x: i32, y: i32, r: i32) -> u8 {
    let radius = f64::from(r);
    let mut hits = 0u32;
    for sy in 0..SUBSAMPLES {
        for sx in 0..SUBSAMPLES {
            let px = f64::from(x) + (f64::from(sx) + 0.5) * SUB_STEP;
            let py = f64::from(y) + (f64::from(sy) + 0.5) * SUB_STEP;
            let dx = px - radius;
            let dy = py - radius;
            if dx * dx + dy * dy <= radius * radius {
                hits += 1;
            }
        }
    }
    ((f64::from(hits) / SUB_COUNT) * 255.0).round() as u8
}

fn bgr(color: u32) -> [u8; 3] {
    [
        ((color >> 16) & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        (color & 0xFF) as u8,
    ]
}

fn luma(c: [u8; 3]) -> f64 {
    0.0722 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.2126 * f64::from(c[2])
}

/// 浅色主题：按亮度向底色靠/离，清掉文字边缘的彩色次像素。
fn shade(bg: [u8; 3], src: [u8; 3], factor: f64) -> [u8; 3] {
    let base = luma(bg);
    let v = (base + (luma(src) - base) * factor).round();
    let v = v.clamp(0.0, 255.0) as u8;
    [v, v, v]
}

/// 深色主题的当前行：底色归零、文字按 ink 归一化后随波浪变亮。
fn ink_on_black(bg: [u8; 3], src: [u8; 3], factor: f64) -> [u8; 3] {
    let base = luma(bg);
    let span = (255.0 - base).max(1.0);
    let ink = ((luma(src) - base) / span * 255.0).clamp(0.0, 255.0);
    let v = (ink * factor).round().clamp(0.0, 255.0) as u8;
    [v, v, v]
}

fn bump(u: f64) -> f64 {
    let u2 = u * u;
    1.0 / (1.0 + u2 * u2)
}

fn wave(x: i32, width: i32, progress: f64) -> f64 {
    let w = f64::from(width);
    let sigma = w * SIGMA_RATIO;
    let center = progress * (w + 6.0 * sigma) - 3.0 * sigma;
    bump((f64::from(x) + 0.5 - center) / sigma)
}

fn gain(x: i32, width: i32, progress: f64, dark: bool) -> f64 {
    let (outside, inside) = if dark {
        (DARK_OUTSIDE, DARK_INSIDE)
    } else {
        (LIGHT_OUTSIDE, LIGHT_INSIDE)
    };
    outside + (inside - outside) * wave(x, width, progress)
}

struct Ripple {
    core: f64,
    ring: f64,
    alpha: f64,
    thick: f64,
    soft: f64,
    halo: f64,
    halo_alpha: f64,
}

fn halo_reveal(t: u64) -> f64 {
    if t < RIPPLE_GROW_MS {
        t as f64 / RIPPLE_GROW_MS as f64
    } else if t < RIPPLE_CORE_BACK_MS {
        1.0 - (t - RIPPLE_GROW_MS) as f64 / (RIPPLE_CORE_BACK_MS - RIPPLE_GROW_MS) as f64
    } else {
        0.0
    }
}

fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(4)
}

fn ripple_state(ms: u64) -> Ripple {
    let t = ms % RIPPLE_CYCLE_MS;
    let core = if t < RIPPLE_GROW_MS {
        let s = t as f64 / RIPPLE_GROW_MS as f64;
        DOT_CORE_RATIO + (DOT_CORE_TROUGH - DOT_CORE_RATIO) * ease_out(s)
    } else if t < RIPPLE_CORE_BACK_MS {
        let s = (t - RIPPLE_GROW_MS) as f64 / (RIPPLE_CORE_BACK_MS - RIPPLE_GROW_MS) as f64;
        DOT_CORE_TROUGH + (DOT_CORE_RATIO - DOT_CORE_TROUGH) * ease_out(s)
    } else {
        DOT_CORE_RATIO
    };

    let (ring, alpha, thick, soft) = if t < RIPPLE_RING_MS {
        let p = t as f64 / RIPPLE_RING_MS as f64;
        let e = ease_out(p);
        (
            DOT_RING_START + (DOT_RING_END - DOT_RING_START) * e,
            DOT_RING_ALPHA * (1.0 - p).powf(0.6),
            DOT_RING_THICK_START + (DOT_RING_THICK_END - DOT_RING_THICK_START) * e,
            DOT_RING_SOFT_MIN + (DOT_RING_SOFT_MAX - DOT_RING_SOFT_MIN) * p,
        )
    } else {
        (DOT_RING_END, 0.0, DOT_RING_THICK_END, DOT_RING_SOFT_MIN)
    };

    let reveal = halo_reveal(t);
    Ripple {
        core,
        ring,
        alpha,
        thick,
        soft,
        halo: core + DOT_HALO_REVEAL * reveal,
        halo_alpha: DOT_HALO_ALPHA_MAX * reveal.powf(0.8),
    }
}

fn dot_bgr() -> [u8; 3] {
    [DOT_RGB[2], DOT_RGB[1], DOT_RGB[0]]
}

/// 把帧表里的覆盖率叠到背景上：与旧版预乘 BGRA 的合成公式逐字节等价。
///
/// 旧版表里存的是 `dot[c] * alpha / 255`（先取整），合成再 `premult + bg * (255-alpha)/255`；
/// 这里按同一顺序先算 `dot[c] * coverage / 255` 再叠底，所以换格式不改像素值。
#[inline]
fn blend_dot(dot: [u8; 3], coverage: u8, background: [u8; 3]) -> [u8; 3] {
    let alpha = u32::from(coverage);
    let keep = 255 - alpha;
    let mut out = [0u8; 3];
    for channel in 0..3 {
        let premultiplied = u32::from(dot[channel]) * alpha / 255;
        out[channel] = (premultiplied + u32::from(background[channel]) * keep / 255).min(255) as u8;
    }
    out
}

fn bake_dot(slot: i32, center: (i32, i32)) -> Option<DotPlot> {
    if slot <= 0 {
        return None;
    }
    let slot_f = f64::from(slot);
    let size = dot_plot_size(slot);
    let count = (size * size) as usize;
    let half = f64::from(size) / 2.0;

    let mut frames = Vec::with_capacity(count * RIPPLE_FRAMES);
    for frame in 0..RIPPLE_FRAMES {
        let ms = frame as u64 * FRAME_MS as u64;
        let state = ripple_state(ms);
        let core_r = state.core * slot_f;
        let ring_r = state.ring * slot_f;
        let half_thick = state.thick / 2.0;
        let soft = state.soft.max(0.6);
        let halo_r = state.halo * slot_f;
        for y in 0..size {
            for x in 0..size {
                let mut halo_hit = 0.0;
                let mut core_hit = 0.0;
                let mut ring_hit = 0.0;
                for sy in 0..SUBSAMPLES {
                    for sx in 0..SUBSAMPLES {
                        let dx = f64::from(x) + (f64::from(sx) + 0.5) * SUB_STEP - half;
                        let dy = f64::from(y) + (f64::from(sy) + 0.5) * SUB_STEP - half;
                        let distance = (dx * dx + dy * dy).sqrt();
                        let edge = halo_r - distance;
                        halo_hit += if edge >= 0.0 {
                            1.0
                        } else {
                            (1.0 + edge / DOT_HALO_SOFT).clamp(0.0, 1.0)
                        };
                        if distance <= core_r {
                            core_hit += 1.0;
                        }
                        // 空心环：ring_r 是内缘，内缘以内一律 0，向外线性衰减。
                        let s = distance - ring_r;
                        if s >= 0.0 {
                            ring_hit += (1.0 - s / (2.0 * half_thick + soft)).clamp(0.0, 1.0);
                        }
                    }
                }
                let halo_cov = halo_hit / SUB_COUNT * state.halo_alpha;
                let core_cov = core_hit / SUB_COUNT * DOT_CORE_ALPHA;
                let ring_cov = ring_hit / SUB_COUNT * state.alpha;
                let coverage = 1.0 - (1.0 - halo_cov) * (1.0 - core_cov) * (1.0 - ring_cov);
                frames.push((coverage * 255.0).round().clamp(0.0, 255.0) as u8);
            }
        }
    }

    Some(DotPlot {
        size,
        left: center.0 - size / 2,
        top: center.1 - size / 2,
        coverage: frames,
    })
}

/// 命中哪一行；分隔线不算可命中行。
fn hit_test(rows: &[MenuRow], x: i32, y: i32) -> Option<usize> {
    if y < 0 || x < 0 {
        return None;
    }
    rows.iter()
        .position(|row| row.kind != RowKind::Separator && y >= row.top && y < row.bottom)
}

fn actionable_rows(rows: &[MenuRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.kind != RowKind::Separator)
        .map(|(index, _)| index)
        .collect()
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn with_state<R>(f: impl FnOnce(&mut MenuState) -> R) -> Option<R> {
    STATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let state = slot.as_mut()?;
        Some(f(state))
    })
}

/// 关闭信号从"带着时间戳入队"到"窗口过程真正处理"的延迟。
///
/// 只在 `LITESOUND_MENU_TRACE` 下写；用来守住"点外部 ≤ 20 ms 收起"这条回归线。
fn log_close_latency(source: &str) {
    if !trace_on() {
        return;
    }
    let now = unsafe { GetTickCount64() };
    let posted = unsafe { GetMessageTime() }.max(0) as u64;
    diag::log(&format!(
        "菜单关闭 {source} 处理延迟 {} ms",
        now.saturating_sub(posted)
    ));
}

fn finish(outcome: Outcome) {
    let (hwnd, fresh) = with_state(|state| {
        let fresh = !state.done;
        if fresh {
            state.result = outcome;
            state.done = true;
        }
        (state.hwnd, fresh)
    })
    .unwrap_or_default();
    if fresh {
        // 先卸钩子再投消息：`finish` 可能来自窗口过程（正常关闭）也可能来自任何异常路径，
        // 钩子必须在这条最短路径上就被移除，不能等主循环退出去清理。
        remove_mouse_hook();
        unsafe {
            // 自己投一条 posted 消息：`WM_ACTIVATE(WA_INACTIVE)` 往往是 sent 的、在消息 API
            // 内部就派发完了，不投这一下循环还在原地阻塞，窗口关不掉。
            let _ = PostMessageW(Some(hwnd), WM_MENU_DONE, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handled = diag::guard("菜单窗口过程", || match msg {
        WM_MOUSEMOVE => {
            on_mouse_move(hwnd, lparam);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let changed = with_state(|state| {
                state.tracking = false;
                set_hover(state, None)
            })
            .unwrap_or(false);
            if changed {
                render_frame(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_left_down(hwnd, lparam);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            on_left_up(hwnd, lparam);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            on_key(wparam);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                log_close_latency("WM_ACTIVATE");
                finish(Outcome::Cancelled);
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            log_close_latency("WM_KILLFOCUS");
            finish(Outcome::Cancelled);
            LRESULT(0)
        }
        WM_MENU_DONE => {
            // 两种来源：`finish()` 自己投的唤醒（此时已经 done，什么都不做）与
            // `WH_MOUSE_LL` 判定"点在菜单外"投来的关闭请求（此时还没 done，必须真的关）。
            if !menu_done() {
                log_close_latency("WH_MOUSE_LL");
                finish(Outcome::Cancelled);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            finish(Outcome::Cancelled);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    });
    handled.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
}

fn client_point(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 as u32 & 0xFFFF) as i16 as i32;
    let y = ((lparam.0 as u32 >> 16) & 0xFFFF) as i16 as i32;
    (x, y)
}

fn on_mouse_move(hwnd: HWND, lparam: LPARAM) {
    let (x, y) = client_point(lparam);
    let (changed, need_track) = with_state(|state| {
        let hover = hit_test(&state.layout.rows, x, y);
        let changed = set_hover(state, hover);
        let need = !state.tracking;
        state.tracking = true;
        (changed, need)
    })
    .unwrap_or((false, false));
    if need_track {
        let mut track = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = unsafe { TrackMouseEvent(&mut track) };
    }
    if changed {
        render_frame(hwnd);
    }
}

fn on_left_down(hwnd: HWND, lparam: LPARAM) {
    let (x, y) = client_point(lparam);
    let changed = with_state(|state| {
        let hit = hit_test(&state.layout.rows, x, y);
        state.down = hit;
        set_hover(state, hit)
    })
    .unwrap_or(false);
    if changed {
        render_frame(hwnd);
    }
}

fn on_left_up(hwnd: HWND, lparam: LPARAM) {
    let (x, y) = client_point(lparam);
    let outcome = with_state(|state| {
        let down = state.down.take();
        let hit = hit_test(&state.layout.rows, x, y);
        if down != hit {
            return None;
        }
        let index = hit?;
        match state.layout.rows[index].kind {
            RowKind::Device(device) => Some(Outcome::Picked(device)),
            RowKind::Exit => Some(Outcome::Exit),
            RowKind::Separator => None,
        }
    })
    .flatten();
    if let Some(outcome) = outcome {
        finish(outcome);
    } else {
        let _ = hwnd;
    }
}

fn on_key(wparam: WPARAM) {
    let key = wparam.0 as u16;
    if key == VK_ESCAPE.0 {
        finish(Outcome::Cancelled);
        return;
    }
    if key == VK_RETURN.0 || key == VK_SPACE.0 {
        if let Some(outcome) = with_state(|state| {
            let rows = actionable_rows(&state.layout.rows);
            let index = state.hover.or_else(|| rows.first().copied())?;
            match state.layout.rows[index].kind {
                RowKind::Device(device) => Some(Outcome::Picked(device)),
                RowKind::Exit => Some(Outcome::Exit),
                RowKind::Separator => None,
            }
        })
        .flatten()
        {
            finish(outcome);
        }
        return;
    }
    if key != VK_UP.0 && key != VK_DOWN.0 {
        return;
    }
    let up = key == VK_UP.0;
    let changed = with_state(|state| {
        let rows = actionable_rows(&state.layout.rows);
        if rows.is_empty() {
            return false;
        }
        let position = state
            .hover
            .and_then(|hover| rows.iter().position(|row| *row == hover));
        let next = match position {
            Some(index) if up => (index + rows.len() - 1) % rows.len(),
            Some(index) => (index + 1) % rows.len(),
            None if up => rows.len() - 1,
            None => 0,
        };
        set_hover(state, Some(rows[next]))
    })
    .unwrap_or(false);
    if changed {
        if let Some(hwnd) = with_state(|state| state.hwnd) {
            render_frame(hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 钩子的判定：矩形内不关、矩形外关，四条边按左闭右开处理。
    ///
    /// 真机点击才走得到 `mouse_hook`，这里守住它唯一的分支条件，避免"点菜单里也关、
    /// 点在边缘上关/不关"这类回归。
    #[test]
    fn hook_point_test_uses_half_open_bounds() {
        let rect = RECT {
            left: 100,
            top: 200,
            right: 400,
            bottom: 500,
        };
        for (x, y) in [(100, 200), (399, 499), (250, 350), (100, 499), (399, 200)] {
            assert!(point_in_menu(&rect, x, y), "({x},{y}) 应判为菜单内");
        }
        for (x, y) in [
            (99, 350),
            (400, 350),
            (250, 199),
            (250, 500),
            (0, 0),
            (1000, 1000),
        ] {
            assert!(!point_in_menu(&rect, x, y), "({x},{y}) 应判为菜单外");
        }
    }

    /// 圆角：四角全透、四边中点与中心满覆盖。
    #[test]
    fn coverage_cuts_the_four_corners() {
        let (width, height) = (336, 215);
        let map = coverage_map(width, height);
        for (x, y) in [
            (0, 0),
            (width - 1, 0),
            (0, height - 1),
            (width - 1, height - 1),
        ] {
            assert_eq!(map[(y * width + x) as usize], 0, "({x},{y}) 没被切掉");
        }
        for (x, y) in [
            (width / 2, 0),
            (width / 2, height - 1),
            (0, height / 2),
            (width - 1, height / 2),
        ] {
            assert_eq!(map[(y * width + x) as usize], 255, "({x},{y}) 被误切");
        }
    }

    /// 圆弧上的像素必须是**部分覆盖**：这是"圆角没有硬台阶"的量化依据。
    #[test]
    fn coverage_corner_is_anti_aliased() {
        let (width, height) = (336, 215);
        let map = coverage_map(width, height);
        let mut partial = 0;
        for y in 0..MENU_RADIUS {
            for x in 0..MENU_RADIUS {
                let value = map[(y * width + x) as usize];
                if value > 0 && value < 255 {
                    partial += 1;
                }
            }
        }
        assert!(
            partial >= 8,
            "左上角只有 {partial} 个半覆盖像素，圆弧会是硬边"
        );
    }

    /// 当前行黑条自带圆角：角上削透、中部不透明、外框留白为 0。
    #[test]
    fn current_row_band_has_its_own_rounded_corners() {
        let width = 336;
        let row_top = MENU_FRAME;
        let band = BandMask::bake(width, row_top, row_top + 55).expect("烘不出黑条掩码");
        let top = row_top + BAND_INSET_Y;
        // 黑条左上角与右上角：必须被圆角削掉（直角块就是问题现场）
        assert_eq!(band.at(BAND_INSET_X, top, width), 0);
        assert_eq!(band.at(width - BAND_INSET_X - 1, top, width), 0);
        // 行内中段满覆盖，不能整行发软
        assert_eq!(band.at(width / 2, top + 27, width), 255);
        // 内缩留白（0..`BAND_INSET_X`）不参与黑条
        assert_eq!(band.at(0, top + 27, width), 0);
        assert_eq!(band.at(width - 1, top + 27, width), 0);
        // 行外不参与
        assert_eq!(band.at(width / 2, row_top - 1, width), 0);
        assert_eq!(band.at(width / 2, row_top + 55, width), 0);
    }

    /// 深色内阴影：槽底 12、上下壁各压到 0、壁长 8 px，且**上下严格镜像**、绝不比槽底更亮。
    #[test]
    fn band_tone_is_a_symmetric_dark_inner_shadow() {
        let width = 336;
        let band = BandMask::bake(width, MENU_FRAME, MENU_FRAME + 55).expect("烘不出黑条掩码");
        let top = band.top;
        let height = band.height;
        assert_eq!(band.tone(top - 1, true), 0);
        assert_eq!(band.tone(top + height, true), 0);
        let wall: Vec<i16> = (0..BAND_WALL_PX)
            .map(|i| band.tone(top + i, true))
            .collect();
        assert_eq!(wall, vec![0, 0, 1, 2, 3, 5, 7, 9], "上壁缓升");
        assert_eq!(band.tone(top + BAND_WALL_PX, true), 12, "槽底");
        for row in 0..height {
            let tone = band.tone(top + row, true);
            assert_eq!(
                tone,
                band.tone(top + height - 1 - row, true),
                "第 {row} 行与镜像行不一致"
            );
            assert!(
                tone <= 12,
                "第 {row} 行色调 {tone} 比槽底亮，会在槽沿外侧读成独立亮线"
            );
        }
        // 下壁是上壁的倒序，末行回到 0（不再有旧版 0x1C/0x3E 的受光）
        let bottom: Vec<i16> = (0..BAND_WALL_PX)
            .map(|i| band.tone(top + height - 1 - i, true))
            .collect();
        assert_eq!(bottom, vec![0, 0, 1, 2, 3, 5, 7, 9]);
    }

    /// 浅色主题同样画成上下镜像的内壁：槽底 -8、两侧壁沿 -16，不再有底缘受光。
    #[test]
    fn band_tone_reverses_in_the_light_theme() {
        let width = 336;
        let band = BandMask::bake(width, MENU_FRAME, MENU_FRAME + 55).expect("烘不出黑条掩码");
        let top = band.top;
        let height = band.height;
        let wall: Vec<i16> = (0..BAND_WALL_PX)
            .map(|i| band.tone(top + i, false))
            .collect();
        assert_eq!(
            wall,
            vec![-16, -16, -15, -15, -14, -13, -11, -10],
            "上壁深线"
        );
        assert_eq!(band.tone(top + BAND_WALL_PX, false), -8, "槽底");
        for row in 0..height {
            let tone = band.tone(top + row, false);
            assert_eq!(
                tone,
                band.tone(top + height - 1 - row, false),
                "第 {row} 行与镜像行不一致"
            );
            assert!(tone <= -8, "第 {row} 行色调 {tone} 比槽底亮");
        }
    }

    /// 离屏合成后的槽色剖面：槽沿下方绝不能出现比菜单底（`0x2C` = 44）更亮的行。
    ///
    /// 旧版底缘两行是 `0x1C` / `0x3E`（28 / 62），末行 62 比 44 亮，这就是"黑槽下方多出一条
    /// 灰线"的来源。这里取槽内中段一列（远离文字、标记与绿点），逐行比对合成结果。
    #[test]
    fn band_bottom_edge_never_exceeds_the_menu_background() {
        let state = bare_state(true);
        let full = RECT {
            left: 0,
            top: 0,
            right: state.layout.width,
            bottom: state.layout.height,
        };
        fill_rect(state.layer.dc, &full, state.layout.palette.bg);
        compose_at(&state, &[full], 0);
        let band = state.band.as_ref().expect("当前行黑槽");
        let width = state.layout.width;
        let background = 0x2C;
        let x = width / 2;
        let mut column = Vec::new();
        for y in band.top..band.top + band.height {
            let index = ((y * width + x) * 4) as usize;
            let red = unsafe { *state.layer.pixels.add(index + 2) };
            assert!(
                red <= background,
                "y={y} 的槽色 {red} 比菜单底 {background} 亮，会读成独立亮线"
            );
            column.push(red);
        }
        // 下壁必须逐行收暗进槽沿，而不是突然出现的一条线
        let wall = BAND_WALL_PX as usize;
        let split = column.len() - wall;
        assert!(
            column[wall..split].iter().all(|value| *value == 12),
            "槽底不是恒定的 12：{:?}",
            &column[wall..split]
        );
        let tail = &column[split..];
        assert_eq!(tail[0], 9, "下壁第一级应从槽底收到 9");
        assert!(
            tail.windows(2).all(|pair| pair[0] >= pair[1]),
            "下壁不是单调收暗：{tail:?}"
        );
        assert_eq!(*tail.last().unwrap(), 0, "槽沿末行应压到 0");
    }

    /// 黑条圆弧必须是半覆盖过渡，不能是硬台阶。
    ///
    /// [`BAND_RADIUS`] = 8 px 的弧长 12.6 px，取"整圈圆弧范围内至少 2 个半覆盖像素"即可；
    /// 采样框用菜单半径来取，保证窗口更大时也不漏掉弧外的过渡像素。
    #[test]
    fn current_row_band_corner_is_anti_aliased() {
        let width = 336;
        let band = BandMask::bake(width, MENU_FRAME, MENU_FRAME + 55).expect("烘不出黑条掩码");
        let top = band.top;
        let mut partial = 0;
        for y in top..top + MENU_RADIUS {
            for x in BAND_INSET_X..BAND_INSET_X + MENU_RADIUS {
                let value = band.at(x, y, width);
                if value > 0 && value < 255 {
                    partial += 1;
                }
            }
        }
        assert!(partial >= 2, "黑条左上角只有 {partial} 个半覆盖像素");
    }

    /// 黑条圆角必须够大（弧上足量半覆盖像素）才不会读成直角，又不该大到把首行上角盖过菜单下角。
    #[test]
    fn band_corner_is_round_and_near_the_menu_corner() {
        let width = 336;
        let band = BandMask::bake(width, MENU_FRAME, MENU_FRAME + 55).expect("烘不出黑条掩码");
        let mut partial = 0;
        for y in band.top..band.top + MENU_RADIUS {
            for x in BAND_INSET_X..BAND_INSET_X + MENU_RADIUS {
                let value = band.at(x, y, width);
                if value > 0 && value < 255 {
                    partial += 1;
                }
            }
        }
        assert!(
            partial >= 10,
            "槽角半径 {BAND_RADIUS} px 只留下 {partial} 个半覆盖像素，真机会读成直角"
        );
        let slot = (BAND_INSET_X + BAND_RADIUS, band.top + BAND_RADIUS);
        let menu = (MENU_RADIUS, MENU_RADIUS);
        let dx = f64::from(slot.0 - menu.0);
        let dy = f64::from(slot.1 - menu.1);
        assert!(
            (dx * dx + dy * dy).sqrt() <= 6.0,
            "槽角圆心 {slot:?} 与菜单角圆心 {menu:?} 相距 {:?} px",
            (dx * dx + dy * dy).sqrt()
        );
    }

    /// 第一行是当前设备时，黑条在角上必须按覆盖率削掉，不能出直角块。
    #[test]
    fn current_row_corner_pixels_are_translucent() {
        let (width, height) = (336, 215);
        let map = coverage_map(width, height);
        // 条目从 (3,3) 起；第一行的黑条左上角在 (3,3)，必须不是满 alpha
        assert!(map[(3 * width + 3) as usize] < 255);
        // 同一条黑条的中部仍是满 alpha，不能把整行都削软
        assert_eq!(map[(30 * width + 30) as usize], 255);
    }

    /// 落位：托盘右下角放不下时向上/向左翻，最终仍在工作区内。
    #[test]
    fn menu_origin_stays_inside_the_work_area() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1568,
        };
        let (x, y) = menu_origin(POINT { x: 2480, y: 1440 }, 336, 215, work);
        assert_eq!((x, y), (2224, 1353));
        assert!(x >= work.left && x + 336 <= work.right);
        assert!(y >= work.top && y + 215 <= work.bottom);
    }

    /// 深色当前行：底色归零，纯白文字保持 255（不然扫光峰值会把字压灰）。
    #[test]
    fn ink_on_black_keeps_the_text_bright_and_the_band_black() {
        let bg = [0x2C, 0x2C, 0x2C];
        assert_eq!(ink_on_black(bg, bg, 1.0), [0, 0, 0]);
        assert_eq!(ink_on_black(bg, [255, 255, 255], 1.0), [255, 255, 255]);
        let dim = ink_on_black(bg, [0xB4, 0xB4, 0xB4], 0.62);
        assert!(dim[0] > 0 && dim[0] < 255);
        assert!(dim[0] < ink_on_black(bg, [0xB4, 0xB4, 0xB4], 1.0)[0]);
    }

    /// 浅色主题按亮度合成，R/G/B 三通道相等（清掉次像素彩边）。
    #[test]
    fn shade_output_is_neutral() {
        let color = shade([0xF0, 0xF0, 0xF0], [0x9A, 0x20, 0xB0], 0.5);
        assert_eq!(color[0], color[1]);
        assert_eq!(color[1], color[2]);
    }

    /// 扫光的带外/带内倍数沿用定稿值，且不越界。
    #[test]
    fn gain_endpoints_follow_the_theme() {
        let width = 400;
        // progress 0.5 时亮带中心落在 x≈200，峰值应贴近带内值
        let peak = gain(200, width, 0.5, true);
        assert!(peak > 0.99, "亮带中心只有 {peak}，扫光太弱");
        // progress 0 时带心在左界外 3σ，x=0 处已接近带外值
        let edge = gain(0, width, 0.0, true);
        assert!((edge - DARK_OUTSIDE).abs() < 0.02, "带外值漂到 {edge}");
        for x in 0..width {
            for step in [0.0, 0.25, 0.5, 0.75] {
                let value = gain(x, width, step, true);
                assert!((DARK_OUTSIDE - 1e-9..=DARK_INSIDE + 1e-9).contains(&value));
                let light = gain(x, width, step, false);
                assert!((LIGHT_INSIDE - 1e-9..=LIGHT_OUTSIDE + 1e-9).contains(&light));
            }
        }
    }

    /// Step 6 探针：合并 DIB 后，整窗像素逐帧指纹必须与合并前一字不差。
    ///
    /// 合并前（两张 DIB，2026-11-04 本机）记录：`combined=0x84a688caed8aa241`，
    /// 每条 `frame NN now=NNN hash=...` 已与合并后逐行比对过。序列完全按
    /// [`render_frame`] 走：当前行每帧先 `paint_row` 画回原始色，再 `compose`。
    ///
    /// 2026-11-04 圆角改造后 `combined` 变为 `0xeedc4b7f85fc260b`：差异只在当前行黑槽的
    /// 四个角（`BAND_RADIUS` 12 → 3，槽角挖空变小），菜单外角/扫光/文字的逐帧值不变。
    ///
    /// 2026-11-04 黑槽内阴影改为上下镜像后 `combined` 变为 `0xb685d15e8ffd30c4`：差异只在
    /// 当前行黑槽的色调（深色槽底 12、上下壁各 8 px 压到 0，去掉底缘 `0x1C`/`0x3E` 受光）。
    #[test]
    #[ignore = "探针：cargo test --release --bin litesound -- --ignored probe_frame_pixel_hash --nocapture"]
    fn probe_frame_pixel_hash() {
        let state = bench_state();
        paint_rows(&state);
        let full = RECT {
            left: 0,
            top: 0,
            right: state.layout.width,
            bottom: state.layout.height,
        };
        compose_at(&state, &[full], 0);
        let anim = animation_rect(&state).expect("动画矩形");
        let current = state.layout.current.expect("当前行");
        let len = (state.layout.width * state.layout.height * 4) as usize;
        let mut combined = 0u64;
        for frame in 0..RIPPLE_FRAMES {
            let now = frame as u64 * FRAME_MS as u64;
            paint_row(&state, current);
            compose_at(&state, &[anim], now);
            let pixels = unsafe { std::slice::from_raw_parts(state.layer.pixels, len) };
            let hash = fnv1a(pixels);
            println!("frame {frame:02} now={now:4} hash={hash:#018x}");
            combined ^= hash;
        }
        println!("combined={combined:#018x}");
    }

    /// 四角覆盖率必须严格镜像：上下翻转 + 左右镜像后逐元素相等。
    ///
    /// 四种常见尺寸都钉一遍（真机 150% 下 336×215 / 347×149；再取一组奇偶混合，
    /// 防止 `height-1-y` 的取整在奇数高时出错）。
    #[test]
    fn coverage_is_mirrored_across_both_axes() {
        for (width, height) in [(336, 215), (347, 149), (337, 216), (400, 300)] {
            let map = coverage_map(width, height);
            let at = |x: i32, y: i32| map[(y * width + x) as usize];
            for y in 0..MENU_RADIUS {
                for x in 0..MENU_RADIUS {
                    let top_left = at(x, y);
                    assert_eq!(
                        top_left,
                        at(width - 1 - x, y),
                        "{width}x{height} 右上角 x={x} y={y}"
                    );
                    assert_eq!(
                        top_left,
                        at(x, height - 1 - y),
                        "{width}x{height} 左下角 x={x} y={y}"
                    );
                    assert_eq!(
                        top_left,
                        at(width - 1 - x, height - 1 - y),
                        "{width}x{height} 右下角 x={x} y={y}"
                    );
                }
            }
        }
    }

    /// 下沿角部不能硬截断：不能出现"某一行从 0 直接跳到 255"。
    ///
    /// 12 px 圆弧在接近水平切点的行里每行只穿一个像素（这是几何上限，不是实现偷懒），
    /// 所以按"每行至少 1 个中间值 + 底边最后 3 行每行至少 2 个 + 整体至少 3 档中间值"来钉。
    #[test]
    fn bottom_corner_alpha_is_a_continuous_ramp() {
        let (width, height) = (336, 215);
        let map = coverage_map(width, height);
        let at = |x: i32, y: i32| map[(y * width + x) as usize];
        let mut distinct = std::collections::BTreeSet::new();
        // 角带第一行（y = height - MENU_RADIUS）在切点处已被整行覆盖，没有中间值，从下一行开始。
        for y in height - MENU_RADIUS + 1..height {
            let mut mids = 0;
            for x in 0..MENU_RADIUS {
                let value = at(x, y);
                if value > 0 && value < 255 {
                    mids += 1;
                    distinct.insert(value);
                }
            }
            assert!(mids >= 1, "下沿第 {y} 行没有任何中间值（0→255 硬跳）");
            if y >= height - 3 {
                assert!(mids >= 2, "底边第 {y} 行只有 {mids} 个中间值");
            }
        }
        assert!(distinct.len() >= 3, "下沿中间值只有 {} 档", distinct.len());
    }

    /// 只剩结构的离屏状态：文字为空、不烘绿点，合成结果里只剩底色与黑槽色调。
    fn bare_state(dark: bool) -> MenuState {
        let (marker_width, marker_height) = mark_geometry().expect("标记槽");
        let (width, row_height) = (336, 55);
        let mut rows = Vec::new();
        let mut y = MENU_FRAME;
        rows.push(MenuRow {
            top: y,
            bottom: y + row_height,
            kind: RowKind::Device(0),
            text: Vec::new(),
            mark: Mark::Blank,
            dim: false,
        });
        y += row_height;
        rows.push(MenuRow {
            top: y,
            bottom: y + MENU_SEPARATOR_PX,
            kind: RowKind::Separator,
            text: Vec::new(),
            mark: Mark::Blank,
            dim: false,
        });
        y += MENU_SEPARATOR_PX;
        rows.push(MenuRow {
            top: y,
            bottom: y + EXIT_ROW_PX,
            kind: RowKind::Exit,
            text: Vec::new(),
            mark: Mark::Blank,
            dim: false,
        });
        y += EXIT_ROW_PX;
        let height = y + MENU_FRAME;
        let layout = Layout {
            width,
            height,
            rows,
            current: Some(0),
            marker_width,
            marker_height,
            dot_center: None,
            dark,
            palette: menu_palette(dark),
        };
        let band = BandMask::bake(width, MENU_FRAME, MENU_FRAME + row_height);
        MenuState {
            hwnd: HWND::default(),
            x: 0,
            y: 0,
            layout,
            layer: Surface::new(width, height).expect("layer"),
            coverage: coverage_map(width, height),
            band,
            dot: None,
            hover: None,
            down: None,
            tracking: false,
            painted: true,
            dirty: Vec::new(),
            result: Outcome::Cancelled,
            done: false,
        }
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// 帧表缓存的复用 / 释放规则（Step 5 的防抖与上限都钉在这两条上）。
    #[test]
    fn dot_cache_reuses_rapid_toggles_and_releases_after_long_menu() {
        reset_dot_cache();
        let slot = 20;
        let center = (20, 20);
        let first = obtain_dot(slot, center).expect("首次烘焙");
        assert_eq!(dot_cache_entries(), 1, "烘焙后应有一份缓存");

        let second = obtain_dot(slot, center).expect("命中缓存");
        assert!(Rc::ptr_eq(&first, &second), "同键第二次应复用同一份帧表");

        // 键变化：整份替换，缓存条数恒为 1（不会越开越多）。
        let other = obtain_dot(slot + 2, (24, 24)).expect("换键烘焙");
        assert_eq!(dot_cache_entries(), 1);
        assert!(!Rc::ptr_eq(&second, &other));

        // 菜单活得很短（连续开关）→ 收起时保留，下次直接复用。
        let now = unsafe { GetTickCount64() };
        settle_dot_cache(now.saturating_sub(200));
        assert_eq!(dot_cache_entries(), 1, "短菜单收起不该丢帧表");
        let reused = obtain_dot(slot + 2, (24, 24)).expect("保留后复用");
        assert!(Rc::ptr_eq(&other, &reused));

        // 正常打开过几秒 → 收起时释放，空闲回到基线；下次重烘。
        settle_dot_cache(now.saturating_sub(DOT_CACHE_RAPID_MS + 500));
        assert_eq!(dot_cache_entries(), 0, "长菜单收起应释放帧表");
        drop((first, second, other, reused));
        reset_dot_cache();
    }

    /// 帧表只存覆盖率：每帧 `size²` 字节、共 51 帧。**150% 档（本机 AppliedDPI=144，slot=20）实测 40×40×51 = 81 600 B（79.7 KiB）**，
    /// 同尺寸旧 BGRA 为 326 400 B（318.8 KiB）。`235 824 → 58 956 B` 是 **125% 档（slot=16 / 34×34）** 的实测；
    /// `529 KB` 是 **64 帧 × 46×46 固定画布 × 4 B** 的旧版本（64 帧早已被 51 帧取代，51 帧 ×46×46 BGRA = 421 KB）。
    #[test]
    fn dot_frames_are_coverage_only() {
        let plot = bake_dot(20, (20, 20)).expect("烘焙绿点帧表");
        let per_frame = (plot.size * plot.size) as usize;
        assert_eq!(plot.coverage.len(), per_frame * RIPPLE_FRAMES);
        assert!(
            plot.coverage.len() <= 130 * 1024,
            "帧表 {} 字节，超过 130 KiB",
            plot.coverage.len()
        );
    }

    /// 新格式的着色必须与旧版预乘 BGRA 逐字节一致（误差 0，不是"1 个灰度级内"）。
    #[test]
    fn dot_blend_matches_premultiplied() {
        let dot = dot_bgr();
        let mut unequal = 0usize;
        for coverage in 0..=255u8 {
            for base in 0..=255u8 {
                let background = [base, base / 2, 255 - base];
                let alpha = u32::from(coverage);
                let keep = 255 - alpha;
                // 旧版烘焙：颜色通道 = dot[c] * alpha / 255（先取整），再用同一条 src-over。
                let mut reference = [0u8; 3];
                for channel in 0..3 {
                    let premultiplied = u32::from(dot[channel]) * alpha / 255;
                    reference[channel] = (premultiplied
                        + u32::from(background[channel]) * keep / 255)
                        .min(255) as u8;
                }
                if blend_dot(dot, coverage, background) != reference {
                    unequal += 1;
                }
            }
        }
        assert_eq!(unequal, 0);
    }

    /// 抽帧剖面对比：覆盖率计算一字未改，抽 5 帧钉住"芯满、四角近乎透明"的剖面。
    #[test]
    fn dot_frame_coverage_profile_is_stable() {
        let plot = bake_dot(20, (20, 20)).expect("烘焙绿点帧表");
        let size = plot.size;
        let center = size / 2;
        let stride = (size * size) as usize;
        for frame in [0usize, 10, 20, 30, 50] {
            let slice = &plot.coverage[frame * stride..(frame + 1) * stride];
            let at = |x: i32, y: i32| slice[(y * size + x) as usize];
            // 芯（DOT_CORE_ALPHA = 1.0）在圆心永远满覆盖，四角远离所有半径因此接近 0。
            assert_eq!(at(center, center), 255, "第 {frame} 帧圆心不是满覆盖");
            assert!(at(0, 0) <= 8, "第 {frame} 帧左上角仍有 {}", at(0, 0));
            assert!(at(0, 0) < at(center, center), "第 {frame} 帧角点不比圆心暗");
        }
    }

    /// 命中测试跳过分隔线，但设备行与"退出"都能命中。
    #[test]
    fn hit_test_skips_the_separator() {
        let rows = vec![
            MenuRow {
                top: 3,
                bottom: 58,
                kind: RowKind::Device(0),
                text: Vec::new(),
                mark: Mark::Blank,
                dim: false,
            },
            MenuRow {
                top: 58,
                bottom: 61,
                kind: RowKind::Separator,
                text: Vec::new(),
                mark: Mark::Blank,
                dim: false,
            },
            MenuRow {
                top: 61,
                bottom: 102,
                kind: RowKind::Exit,
                text: Vec::new(),
                mark: Mark::Blank,
                dim: false,
            },
        ];
        assert_eq!(hit_test(&rows, 10, 20), Some(0));
        assert_eq!(hit_test(&rows, 10, 59), None);
        assert_eq!(hit_test(&rows, 10, 80), Some(2));
        assert_eq!(hit_test(&rows, 10, 2), None);
    }

    /// 单帧成本的离线基准（不提交上屏时间，只量 CPU 的"画 + 合成"两段）。
    ///
    /// `cargo test --release --bin litesound -- --ignored bench_frame_cost --nocapture`
    fn bench_state() -> MenuState {
        let (_, px) = menu_font();
        let (marker_width, marker_height) = mark_geometry().expect("标记槽");
        let row_height = menu_row_height(px.max(1));
        let mut rows: Vec<MenuRow> = Vec::new();
        let mut y = MENU_FRAME;
        for index in 0..3 {
            rows.push(MenuRow {
                top: y,
                bottom: y + row_height,
                kind: RowKind::Device(index),
                text: wide(&format!("扬声器 (Senary Audio) {index}")),
                mark: Mark::Blank,
                dim: index != 0,
            });
            y += row_height;
        }
        rows.push(MenuRow {
            top: y,
            bottom: y + MENU_SEPARATOR_PX,
            kind: RowKind::Separator,
            text: Vec::new(),
            mark: Mark::Blank,
            dim: false,
        });
        y += MENU_SEPARATOR_PX;
        rows.push(MenuRow {
            top: y,
            bottom: y + EXIT_ROW_PX,
            kind: RowKind::Exit,
            text: wide("退出"),
            mark: Mark::Blank,
            dim: false,
        });
        y += EXIT_ROW_PX;

        let width = 336;
        let height = y + MENU_FRAME;
        let layout = Layout {
            width,
            height,
            rows,
            current: Some(0),
            marker_width,
            marker_height,
            dot_center: Some((
                MENU_FRAME + MENU_PAD_L + marker_height / 2,
                MENU_FRAME + row_height / 2,
            )),
            dark: true,
            palette: menu_palette(true),
        };
        let band = BandMask::bake(width, MENU_FRAME, MENU_FRAME + row_height);
        let dot = bake_dot(marker_height, layout.dot_center.unwrap()).map(Rc::new);
        MenuState {
            hwnd: HWND::default(),
            x: 0,
            y: 0,
            layout,
            layer: Surface::new(width, height).expect("layer"),
            coverage: coverage_map(width, height),
            band,
            dot,
            hover: None,
            down: None,
            tracking: false,
            painted: true,
            dirty: Vec::new(),
            result: Outcome::Cancelled,
            done: false,
        }
    }

    #[test]
    #[ignore = "基准：cargo test --release --bin litesound -- --ignored bench_frame_cost --nocapture"]
    fn bench_frame_cost() {
        // 帧表换格式的直接收益：同一份绿点，旧版 4 字节/像素 vs 新版 1 字节覆盖率的表大小与烘焙耗时。
        let (marker_width, marker_height) = mark_geometry().expect("标记槽");
        let center = (MENU_FRAME + MENU_PAD_L + marker_width / 2, MENU_FRAME + 20);
        let mut bake_times = Vec::new();
        let mut table = None;
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            let plot = bake_dot(marker_height, center).expect("烘焙绿点帧表");
            bake_times.push(t0.elapsed().as_secs_f64() * 1000.0);
            table = Some(plot);
        }
        bake_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if let Some(plot) = table {
            let pixels = (plot.size * plot.size) as usize * RIPPLE_FRAMES;
            println!(
                "帧表 {}x{} × {RIPPLE_FRAMES} 帧：覆盖率 {} 字节（旧版 BGRA {} 字节，省 {} KB）；烘焙中位 {:.2} ms",
                plot.size,
                plot.size,
                pixels,
                pixels * 4,
                (pixels * 3) / 1024,
                bake_times[2]
            );
        }

        let state = bench_state();
        let width = state.layout.width;
        let height = state.layout.height;
        let mut paint_times = Vec::new();
        let mut compose_times = Vec::new();
        for _ in 0..60 {
            let t0 = std::time::Instant::now();
            paint_rows(&state);
            paint_times.push(t0.elapsed().as_secs_f64() * 1000.0);
            let t1 = std::time::Instant::now();
            compose(
                &state,
                &[RECT {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                }],
            );
            compose_times.push(t1.elapsed().as_secs_f64() * 1000.0);
        }
        paint_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        compose_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "窗口 {width}x{height}：全量 paint 中位 {:.2} ms；全量 compose 中位 {:.2} ms；合计 {:.2} ms",
            paint_times[30],
            compose_times[30],
            paint_times[30] + compose_times[30]
        );

        // 增量路径：每帧只重画当前行 + 只合成动画矩形。
        let anim = animation_rect(&state).expect("动画矩形");
        let mut incremental = Vec::new();
        for _ in 0..60 {
            let t0 = std::time::Instant::now();
            paint_row(&state, 0);
            compose(&state, &[anim]);
            incremental.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        incremental.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "增量路径（当前行 {}x{} + 动画矩形 {}x{}）：中位 {:.2} ms / P90 {:.2} ms",
            anim.right - anim.left,
            anim.bottom - anim.top,
            anim.right - anim.left,
            anim.bottom - anim.top,
            incremental[30],
            incremental[54]
        );
    }

    fn intersects(a: &RECT, b: &RECT) -> bool {
        a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
    }

    fn contains(outer: &RECT, inner: &RECT) -> bool {
        inner.left >= outer.left
            && inner.right <= outer.right
            && inner.top >= outer.top
            && inner.bottom <= outer.bottom
    }

    /// 显式 cell 布局的不变量：任意 marks 组合 × 3 个 DPI × 若干行高。
    ///
    /// 这里钉住四件事：两个 mark 的 cell 不相交、mark 不与文字区相交、mark 全在标记列内、
    /// 环外沿不越出点 cell。任何一条红，就说明又回到了"靠偏移量错开"的旧路。
    #[test]
    fn mark_cells_never_intersect_across_marks_dpi_and_row_heights() {
        let marks = [Mark::Blank, Mark::Dot, Mark::Bluetooth, Mark::Both];
        // 100% / 125% / 150%：系统菜单字 12 逻辑 × 1.5556 后取整，再乘缩放。
        for px in [19, 24, 29] {
            let glyph_cell = crate::tray::mark_height(px);
            let dot_cell = dot_cell_size(glyph_cell);
            let max_column = mark_column_width(Mark::Both, dot_cell, glyph_cell);
            let base_row = menu_row_height(px);
            for row_height in [base_row, 55, 72] {
                let top = MENU_FRAME;
                let bottom = top + row_height;
                let column_left = MENU_FRAME + MENU_PAD_L;
                let column = RECT {
                    left: column_left,
                    top,
                    right: column_left + max_column,
                    bottom,
                };
                let text_left = column.right + MENU_GAP;
                let text = RECT {
                    left: text_left,
                    top,
                    right: column.right + 200,
                    bottom,
                };

                for mark in marks {
                    assert!(
                        mark_column_width(mark, dot_cell, glyph_cell) <= max_column,
                        "px={px} row={row_height} {mark:?} 的列宽超过全场最大列宽"
                    );
                    let (dot, glyph) = mark_cells(mark, top, bottom, dot_cell, glyph_cell);
                    for cell in [dot.as_ref(), glyph.as_ref()].into_iter().flatten() {
                        assert!(
                            contains(&column, cell),
                            "px={px} row={row_height} {mark:?} 的 mark 越出标记列：{cell:?} 不在 {column:?}"
                        );
                        assert!(
                            cell.right <= text_left,
                            "px={px} row={row_height} {mark:?} 的 mark 压到文字区"
                        );
                        assert!(!intersects(cell, &text), "mark 与文字区相交");
                    }
                    if let (Some(dot), Some(glyph)) = (dot.as_ref(), glyph.as_ref()) {
                        assert!(
                            !intersects(dot, glyph),
                            "px={px} row={row_height} 点 cell {dot:?} 与符号 cell {glyph:?} 相交"
                        );
                        assert!(
                            dot.right + MARK_GAP <= glyph.left,
                            "px={px} row={row_height} 两 cell 之间没有留出 MARK_GAP"
                        );
                    }
                    if let Some(dot) = dot.as_ref() {
                        let reach = dot_reach(glyph_cell);
                        assert!(
                            f64::from(dot.right - dot.left) >= 2.0 * reach + 1.0,
                            "px={px} row={row_height} 点 cell 装不下环外沿：宽 {}，需要 ≥ {:.1}",
                            dot.right - dot.left,
                            2.0 * reach + 1.0
                        );
                        assert!(
                            dot_plot_size(glyph_cell) <= dot.right - dot.left,
                            "动画帧表比点 cell 还大，环会溢出"
                        );
                    }
                }
            }
        }
    }

    /// 诊断：真画一遍当前蓝牙行，扫描符号 cell 里到底有没有墨迹。
    ///
    /// `cargo test --bin litesound -- --ignored dump_bluetooth_cell --nocapture`
    #[test]
    #[ignore = "诊断：扫描符号 cell 的墨迹"]
    fn dump_bluetooth_cell() {
        // 复现生产：进程先声明允许深色菜单，`menu_ink()` 才会给白色墨迹。
        crate::tray::enable_dark_menu_mode();
        println!(
            "menu_is_dark={}  AppsUseLightTheme={}",
            menu_is_dark(),
            crate::tray::apps_use_light_theme_for_test()
        );
        let (font, _px) = menu_font();
        // 强制 150% 的真实值：字 29 px → 槽 20、点 cell 41、Both 列宽 65。
        let marker_height = 20;
        let marker_width =
            mark_column_width(Mark::Both, dot_cell_size(marker_height), marker_height);
        let row_height = 55;
        let mut y = MENU_FRAME;
        let mut rows = Vec::new();
        rows.push(MenuRow {
            top: y,
            bottom: y + row_height,
            kind: RowKind::Device(0),
            text: wide("蓝牙扬声器 (Senary Audio)"),
            mark: Mark::Both,
            dim: false,
        });
        y += row_height;
        rows.push(MenuRow {
            top: y,
            bottom: y + MENU_SEPARATOR_PX,
            kind: RowKind::Separator,
            text: Vec::new(),
            mark: Mark::Blank,
            dim: false,
        });
        y += MENU_SEPARATOR_PX;
        rows.push(MenuRow {
            top: y,
            bottom: y + EXIT_ROW_PX,
            kind: RowKind::Exit,
            text: wide("退出"),
            mark: Mark::Blank,
            dim: false,
        });
        y += EXIT_ROW_PX;

        let width = item_width(text_width(&rows[0].text, font), marker_width) + MENU_FRAME * 2;
        let height = y + MENU_FRAME;
        let layout = Layout {
            width,
            height,
            rows,
            current: Some(0),
            marker_width,
            marker_height,
            dot_center: Some((
                MENU_FRAME + MENU_PAD_L + dot_cell_size(marker_height) / 2,
                MENU_FRAME + row_height / 2,
            )),
            dark: true,
            palette: menu_palette(true),
        };
        let band = BandMask::bake(width, MENU_FRAME, MENU_FRAME + row_height);
        let dot = bake_dot(marker_height, layout.dot_center.unwrap()).map(Rc::new);
        let state = MenuState {
            hwnd: HWND::default(),
            x: 0,
            y: 0,
            layout,
            layer: Surface::new(width, height).expect("layer"),
            coverage: coverage_map(width, height),
            band,
            dot,
            hover: None,
            down: None,
            tracking: false,
            painted: true,
            dirty: Vec::new(),
            result: Outcome::Cancelled,
            done: false,
        };

        paint_rows(&state);
        let row = &state.layout.rows[0];
        let dot_cell = dot_cell_size(state.layout.marker_height);
        let glyph_cell = state.layout.marker_height;
        let (dot_rect, glyph_rect) =
            mark_cells(row.mark, row.top, row.bottom, dot_cell, glyph_cell);
        println!(
            "行 [{},{}] mark={:?} marker_width={marker_width} marker_height={marker_height} menu={}x{height}",
            row.top, row.bottom, row.mark, state.layout.width
        );
        println!("dot cell={dot_rect:?}");
        println!("glyph 目标 cell={glyph_rect:?}");
        println!(
            "glyph_bitmap={:?}（{}x{}）",
            glyph_bitmap(glyph_cell, glyph_cell).is_some(),
            glyph_cell,
            glyph_cell
        );

        fn scan_cell(state: &MenuState, width: i32, cell: &RECT) -> (u32, i32, i32, i32, i32) {
            // 同一行、cell 左边 gap 里的像素当对照：黑条/扫光/覆盖率对它和符号的作用完全一样，
            // 只有墨迹会让两者不同。
            let control_x = cell.left - 2;
            let (mut left, mut top, mut right, mut bottom, mut count) =
                (cell.right, cell.bottom, -1, -1, 0u32);
            for yy in cell.top..cell.bottom {
                let ci = ((yy * width + control_x) * 4) as usize;
                let control = unsafe {
                    [
                        *state.layer.pixels.add(ci),
                        *state.layer.pixels.add(ci + 1),
                        *state.layer.pixels.add(ci + 2),
                    ]
                };
                for xx in cell.left..cell.right {
                    let i = ((yy * width + xx) * 4) as usize;
                    let pixel = unsafe {
                        [
                            *state.layer.pixels.add(i),
                            *state.layer.pixels.add(i + 1),
                            *state.layer.pixels.add(i + 2),
                        ]
                    };
                    let delta = (0..3)
                        .map(|k| pixel[k].abs_diff(control[k]))
                        .max()
                        .unwrap_or(0);
                    if delta > 16 {
                        count += 1;
                        left = left.min(xx);
                        top = top.min(yy);
                        right = right.max(xx);
                        bottom = bottom.max(yy);
                    }
                }
            }
            (count, left, top, right, bottom)
        }

        let Some(cell) = glyph_rect else {
            return;
        };
        if let Some(px) = crate::tray::glyph_cell_pixels_for_test(
            glyph_cell,
            glyph_cell,
            crate::tray::mark_share_for_test(),
        ) {
            let count = px.as_chunks::<4>().0.iter().filter(|p| p[3] > 0).count();
            let max = px
                .as_chunks::<4>()
                .0
                .iter()
                .max_by_key(|p| p[3])
                .map(|p| [p[0], p[1], p[2], p[3]])
                .unwrap_or([0; 4]);
            println!("cell 预乘像素 alpha>0 共 {count} 个，最大 alpha 样本 BGRA={max:?}");
            let mut shown = 0;
            for y in 0..glyph_cell {
                for x in 0..glyph_cell {
                    let i = ((y * glyph_cell + x) * 4) as usize;
                    let p = [px[i], px[i + 1], px[i + 2], px[i + 3]];
                    if p[3] > 0 && shown < 24 {
                        println!("  bitmap ({x},{y}) BGRA={p:?}");
                        shown += 1;
                    }
                }
            }
        }
        if let Some(bits) = crate::tray::glyph_bitmap_bits_for_test(glyph_cell, glyph_cell) {
            for (x, y) in [(9, 3), (8, 10), (8, 9), (9, 10)] {
                let i = ((y * glyph_cell + x) * 4) as usize;
                println!(
                    "DIB ({x},{y}) BGRA={:?}（全局 ({},{})）",
                    [bits[i], bits[i + 1], bits[i + 2], bits[i + 3]],
                    glyph_rect.unwrap().left + x,
                    glyph_rect.unwrap().top + y,
                );
            }
        }

        let sample_y = (cell.top + cell.bottom) / 2;
        let sample_before: Vec<(i32, u8, u8, u8, u8)> = (cell.left..cell.right)
            .map(|x| {
                let i = ((sample_y * width + x) * 4) as usize;
                unsafe {
                    (
                        x,
                        *state.layer.pixels.add(i),
                        *state.layer.pixels.add(i + 1),
                        *state.layer.pixels.add(i + 2),
                        *state.layer.pixels.add(i + 3),
                    )
                }
            })
            .collect();
        let (c0, l0, t0, r0, b0) = scan_cell(&state, width, &cell);
        println!("compose 前：不同像素 {c0}，bbox=[{l0},{t0}]..[{r0},{b0}]");

        let full = RECT {
            left: 0,
            top: 0,
            right: state.layout.width,
            bottom: state.layout.height,
        };
        compose_at(&state, &[full], 0);
        let (c1, l1, t1, r1, b1) = scan_cell(&state, width, &cell);
        println!("compose 后：不同像素 {c1}，bbox=[{l1},{t1}]..[{r1},{b1}]");
        let sample_after: Vec<(i32, u8)> = (cell.left..cell.right)
            .map(|x| unsafe {
                (
                    x,
                    *state
                        .layer
                        .pixels
                        .add(((sample_y * width + x) * 4 + 2) as usize),
                )
            })
            .collect();
        println!("y={sample_y} 前 BGRA={sample_before:?}");
        println!("y={sample_y} 后 R={sample_after:?}");
    }

    /// 诊断：AlphaBlend 到底把"直 alpha 白墨"读成什么（对照：预乘 alpha 源）。
    ///
    /// `cargo test --bin litesound -- --ignored dump_alphablend_probe --nocapture`
    #[test]
    #[ignore = "诊断：AlphaBlend 源 alpha 语义"]
    fn dump_alphablend_probe() {
        use windows::Win32::Graphics::Gdi::AlphaBlend;
        fn probe(name: &str, source: [[u8; 4]; 4]) {
            let dest = Surface::new(8, 8).expect("dest");
            fill_rect(
                dest.dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: 8,
                    bottom: 8,
                },
                0x2C2C2C,
            );
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 2,
                    biHeight: -2,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let src = unsafe {
                CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).expect("src")
            };
            let flat: Vec<u8> = source.iter().flatten().copied().collect();
            unsafe { std::ptr::copy_nonoverlapping(flat.as_ptr(), bits as *mut u8, flat.len()) };
            let mem = unsafe { CreateCompatibleDC(Some(dest.dc)) };
            let previous = unsafe { SelectObject(mem, HGDIOBJ(src.0)) };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            unsafe {
                let _ = AlphaBlend(dest.dc, 1, 1, 2, 2, mem, 0, 0, 2, 2, blend);
                SelectObject(mem, previous);
                let _ = DeleteDC(mem);
                let _ = DeleteObject(HGDIOBJ(src.0));
            }
            let read = |x: i32, y: i32| -> [u8; 4] {
                let i = ((y * 8 + x) * 4) as usize;
                unsafe {
                    [
                        *dest.pixels.add(i),
                        *dest.pixels.add(i + 1),
                        *dest.pixels.add(i + 2),
                        *dest.pixels.add(i + 3),
                    ]
                }
            };
            println!(
                "{name}: 源 {source:?} → 目标 (1,1)={:?} (2,2)={:?} (1,2)={:?} (2,1)={:?}",
                read(1, 1),
                read(2, 2),
                read(1, 2),
                read(2, 1)
            );
        }
        probe(
            "直 alpha 白",
            [
                [255, 255, 255, 128],
                [255, 255, 255, 255],
                [255, 255, 255, 192],
                [0, 0, 0, 0],
            ],
        );
        probe(
            "预乘 白",
            [
                [128, 128, 128, 128],
                [255, 255, 255, 255],
                [192, 192, 192, 192],
                [0, 0, 0, 0],
            ],
        );
    }

    /// 诊断：打印当前行黑槽左上/右下圆角 12×12 的覆盖率与合成后的色值。
    ///
    /// `cargo test --bin litesound -- --ignored dump_band_corner --nocapture`
    #[test]
    #[ignore = "诊断：打印黑槽圆角覆盖率矩阵"]
    fn dump_band_corner() {
        let state = bare_state(true);
        let width = state.layout.width;
        let full = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: state.layout.height,
        };
        fill_rect(state.layer.dc, &full, state.layout.palette.bg);
        compose_at(&state, &[full], 0);
        let band = state.band.as_ref().expect("当前行黑槽");
        let radius = BAND_RADIUS;
        let left = BAND_INSET_X;
        println!(
            "band left={left} top={} height={} right={} radius={radius} 左上圆心=({},{}) 菜单半径={MENU_RADIUS} 菜单圆心=({MENU_RADIUS},{MENU_RADIUS})",
            band.top,
            band.height,
            width - BAND_INSET_X,
            left + radius,
            band.top + radius,
        );
        let mut partial = (0u32, 0u32); // (半覆盖像素数, 满覆盖像素数)
        for y in band.top..band.top + band.height {
            for x in left..width - BAND_INSET_X {
                match band.at(x, y, width) {
                    0 => {}
                    255 => partial.1 += 1,
                    _ => partial.0 += 1,
                }
            }
        }
        println!(
            "整条：满覆盖 {} px，半覆盖 {} px（弧长上界约 {:.1} px）",
            partial.1,
            partial.0,
            std::f64::consts::FRAC_PI_2 * f64::from(radius)
        );
        for (name, origin) in [
            ("左上", (left, band.top)),
            (
                "右下",
                (width - BAND_INSET_X - 1, band.top + band.height - 1),
            ),
        ] {
            let (x0, y0) = origin;
            let (sx, sy) = if name == "左上" {
                (x0 - 2, y0 - 2)
            } else {
                (x0 - 9, y0 - 9)
            };
            println!("{name}角 12×12 覆盖率（原点 {sx},{sy}）：");
            let mut partial8 = 0u32;
            for y in sy..sy + 12 {
                let row: Vec<u8> = (sx..sx + 12)
                    .map(|x| {
                        let value = band.at(x, y, width);
                        if value > 0 && value < 255 {
                            partial8 += 1;
                        }
                        value
                    })
                    .collect();
                println!("{row:?}");
            }
            println!("{name}角 12×12 合成后 R 通道：");
            for y in sy..sy + 12 {
                let row: Vec<u8> = (sx..sx + 12)
                    .map(|x| unsafe { *state.layer.pixels.add(((y * width + x) * 4 + 2) as usize) })
                    .collect();
                println!("{row:?}");
            }
            println!("{name}角 12×12 合成后 alpha：");
            for y in sy..sy + 12 {
                let row: Vec<u8> = (sx..sx + 12)
                    .map(|x| unsafe { *state.layer.pixels.add(((y * width + x) * 4 + 3) as usize) })
                    .collect();
                println!("{row:?}");
            }
            if name == "左上" {
                println!("对照：半径 3（上一版）同一角落 12×12 覆盖率（同一 4×4 采样）：");
                let mut partial3 = 0u32;
                for y in sy..sy + 12 {
                    let row: Vec<u8> = (sx..sx + 12)
                        .map(|x| {
                            let mut hits = 0u32;
                            for suby in 0..SUBSAMPLES {
                                for subx in 0..SUBSAMPLES {
                                    let px = f64::from(x) + (f64::from(subx) + 0.5) * SUB_STEP;
                                    let py = f64::from(y) + (f64::from(suby) + 0.5) * SUB_STEP;
                                    if inside_rounded_rect(
                                        px,
                                        py,
                                        f64::from(left),
                                        f64::from(band.top),
                                        f64::from(width - BAND_INSET_X),
                                        f64::from(band.top + band.height),
                                        3.0,
                                    ) {
                                        hits += 1;
                                    }
                                }
                            }
                            let value = ((f64::from(hits) / SUB_COUNT) * 255.0).round() as u8;
                            if value > 0 && value < 255 {
                                partial3 += 1;
                            }
                            value
                        })
                        .collect();
                    println!("{row:?}");
                }
                println!("半径 3 该角落半覆盖像素 = {partial3}（半径 8 为 {partial8}）");
            }
        }
    }

    /// 诊断输出：用数值复现"旧布局当前蓝牙行"与"新布局同行"的标记区，不截图。
    ///
    /// `cargo test --bin litesound -- --ignored dump_mark_layout --nocapture`
    #[test]
    #[ignore = "诊断：打印新旧标记区几何"]
    fn dump_mark_layout() {
        for slot in [13, 16, 20] {
            let old_width = slot * 2 + 4;
            let old_center = MENU_FRAME + MENU_PAD_L + slot / 2;
            let old_reach = dot_reach(slot);
            let old_text = MENU_FRAME + MENU_PAD_L + old_width + MENU_GAP;
            let (glyph_left, glyph_right) =
                crate::tray::legacy_mark_pixels(Mark::Bluetooth, old_width, slot)
                    .map(|pixels| {
                        let (mut left, mut right) = (old_width, -1);
                        for y in 0..slot {
                            for x in 0..old_width {
                                if pixels[((y * old_width + x) * 4 + 3) as usize] != 0 {
                                    left = left.min(x);
                                    right = right.max(x);
                                }
                            }
                        }
                        (
                            MENU_FRAME + MENU_PAD_L + left,
                            MENU_FRAME + MENU_PAD_L + right,
                        )
                    })
                    .unwrap_or((-1, -1));

            let dot_cell = dot_cell_size(slot);
            let column = mark_column_width(Mark::Both, dot_cell, slot);
            let new_center = MENU_FRAME + MENU_PAD_L + dot_cell / 2;
            let new_reach = dot_reach(slot);
            let (new_dot, new_glyph) =
                mark_cells(Mark::Both, MENU_FRAME, MENU_FRAME + 55, dot_cell, slot);
            let new_text = MENU_FRAME + MENU_PAD_L + column + MENU_GAP;

            println!("===== 槽高 {slot} px（槽 = 符号 cell 边长）=====");
            println!(
                "[旧] 列宽 {old_width}：点圆心 x={old_center}，芯 r={:.1}、晕 r={:.1}、环外沿 r={old_reach:.1}；\
                 符号墨迹 x=[{glyph_left},{glyph_right}]；文字 x={old_text}",
                DOT_CORE_RATIO * f64::from(slot),
                (DOT_CORE_RATIO + DOT_HALO_REVEAL) * f64::from(slot),
            );
            let overlap = (old_center as f64 + old_reach) - glyph_left as f64;
            println!(
                "[旧] 环外沿右缘 x={:.1} 与符号墨迹左缘 x={glyph_left}：{}",
                old_center as f64 + old_reach,
                if overlap > 0.0 {
                    format!("相交 {overlap:.1} px")
                } else {
                    format!("间距 {:.1} px", -overlap)
                }
            );
            println!(
                "[新] 点 cell={dot_cell}（x=[{},{}]），点圆心 x={new_center}，环外沿 r={new_reach:.1}；\
                 符号 cell={slot}（x=[{},{}]）；文字 x={new_text}",
                new_dot.expect("点 cell").left,
                new_dot.expect("点 cell").right,
                new_glyph.expect("符号 cell").left,
                new_glyph.expect("符号 cell").right,
            );
            let ring_right = new_center as f64 + new_reach;
            println!(
                "[新] 环外沿右缘 x={ring_right:.1} 与符号 cell 左缘 x={}：间距 {:.1} px",
                new_glyph.expect("符号 cell").left,
                f64::from(new_glyph.expect("符号 cell").left) - ring_right,
            );
            println!(
                "[新] 仅点行文字 x={}；文字起点：{old_text} → {new_text}（{:+.0} px）；\
                 标记列宽：{old_width} → {column}（{:+.0} px）",
                MENU_FRAME + MENU_PAD_L + dot_cell + MENU_GAP,
                (new_text - old_text) as f64,
                (column - old_width) as f64,
            );
        }
    }
}
