//! 横幅窗口：DIB 分层窗口 + 淡出定时器。
//!
//! 形态：`WS_POPUP` 的**分层窗口**（`WS_EX_LAYERED` + `UpdateLayeredWindow` 提交完整 DIB），
//! 背景与文字先画到 32 位位图，圆角靠 `SetWindowRgn` 裁（规格 4.2；不用色键抠角）。
//!
//! 窗口**懒创建并复用**：横幅每次切换都要弹，反复建/销窗口与字体只会把常驻开销推高，
//! 而本项目唯一的验收指标就是常驻内存。
//!
//! 时间源只有两个定时器：停留 1600 ms → 40 ms 一步减 alpha（规格 4.3）。定时器回调回到
//! 本窗口的消息队列，也就是托盘线程，所以这里可以安全地碰 UI。

use crate::diag;
use crate::fade;
use crate::geometry::{self, BannerRect};
use crate::naming;
use core::ffi::c_void;
use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, AtomicU8, Ordering};
use std::sync::OnceLock;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateRoundRectRgn, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, FillRect, GetDC, GetTextExtentPoint32W, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, SetWindowRgn, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY, DIB_RGB_COLORS, DT_CENTER,
    DT_END_ELLIPSIS, DT_SINGLELINE, DT_VCENTER, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION,
    FW_BOLD, GB2312_CHARSET, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowRect, KillTimer,
    RegisterClassW, SetTimer, ShowWindow, SystemParametersInfoW, TranslateMessage,
    UpdateLayeredWindow, HCURSOR, HICON, MSG, SPI_GETWORKAREA, SWP_NOACTIVATE, SW_HIDE,
    SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ULW_ALPHA, WINDOWPOS, WM_ACTIVATE,
    WM_TIMER, WM_WINDOWPOSCHANGING, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// 窗口类名。与托盘窗口分开：类只服务横幅，不必和托盘互相迁就。
const CLASS_NAME: PCWSTR = w!("LiteSoundBannerWindow");

/// 停留定时器 / 淡出定时器的 id。同属一个窗口，靠 id 区分。
const TIMER_HOLD: usize = 1;
const TIMER_FADE: usize = 2;

/// `--bannertest` 的自杀定时器 id。**线程定时器**（`SetTimer(None, ..)`），不挂在窗口上：
/// 它投递的 `WM_TIMER` 的 `hwnd` 为空，横幅窗口过程不会看到它。
const TIMER_EXIT: usize = 3;

/// `--bannertest` 的存在时长（规格 2.2）。该模式没有标题栏、收不到 Alt+F4，
/// 不自杀就会留下一个只能杀进程的常驻窗口。
const BANNERTEST_MS: u32 = 5000;

/// 停留时长（规格 4.3）。
const HOLD_MS: u32 = 1600;

/// 淡出步进周期（规格 4.3）。
const FADE_MS: u32 = 40;

/// 背景色 RGB(28,28,32)（规格 4.2）。
const BG_COLOR: COLORREF = COLORREF(rgb(28, 28, 32));

/// 文字纯白（规格 4.2）。
const TEXT_COLOR: COLORREF = COLORREF(rgb(255, 255, 255));

/// 字号：`13 × dpi_scale × 96/72` 像素（规格 4.2，即 13 pt 在当前 DPI 下的像素高）。
const FONT_PT: f64 = 13.0;

/// 字体名（规格 4.2）。
const FACE: &str = "微软雅黑";

/// `COLORREF` 是 `0x00BBGGRR`，不是直觉上的 RGB 顺序。
const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

/// 横幅窗口句柄，0 表示还没建。懒创建后一直复用（单实例、单托盘线程）。
static BANNER: AtomicIsize = AtomicIsize::new(0);

/// 当前 alpha。只在托盘线程读写，用原子量只是为了有个能跨窗口过程访问的 static。
static ALPHA: AtomicU8 = AtomicU8::new(fade::FADE_START);

/// 窗口类是否已注册。`OnceLock` 存的是"注册成功了"这件事：注册失败时也把它定成 `false`，
/// 免得每次切换都重试一次注定失败的注册、还往日志里刷同一行。
static CLASS_READY: OnceLock<bool> = OnceLock::new();

thread_local! {
    static SURFACE: RefCell<Option<LayeredSurface>> = const { RefCell::new(None) };
}

struct LayeredSurface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pixels: *mut u32,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
}

impl Drop for LayeredSurface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

/// 显示一条横幅。
///
/// 文本**先截断到 60 字符**，测量与绘制用的都是截断后的那一串（坑 #4）：量原文却画截断
/// 文本，窗口会按长文字开、按短文字画，而"是否被裁掉"的自检永远不响。
pub fn show(text: &str) {
    let Some(hwnd) = ensure_window() else {
        return;
    };
    let Some(work) = work_area() else {
        return;
    };
    let work_w = work.right - work.left;
    let work_h = work.bottom - work.top;
    let scale = dpi_scale(hwnd);

    // 放不下就**不弹**并记日志（规格 4.3）：弹出屏幕外的横幅与没弹是同一件事，但只有前者
    // 会留下证据，否则真机上只表现为"切换了但什么都没弹"。
    if !geometry::banner_fits(work_w, work_h, scale) {
        diag::log(&format!(
            "横幅：工作区 {work_w}x{work_h} 放不下最小可用横幅，本次不弹"
        ));
        return;
    }

    let shown = naming::truncate_chars(text, naming::MAX_CHARS);
    let Some((text_w, text_h)) = measure(shown, scale) else {
        diag::log("横幅：文字测量失败，本次不弹");
        return;
    };
    let rect = geometry::banner_geometry(text_w, text_h, work_w, work_h, scale);

    // 重弹时把在跑的定时器撤掉：旧定时器会把新横幅提前淡掉或提前隐藏
    kill_timers(hwnd);

    // 先看清窗口当前矩形：ULW 一提交，当前矩形就变成目标矩形，之后再比就没意义。
    // 差异只决定"要不要重设圆角区域"——位置与画面每回都提交（规格 4.6）
    let change = window_change(hwnd, &rect, &work);

    // 位置、尺寸、完整画面由 `render_and_update` 里的 `UpdateLayeredWindow` 一次原子提交：
    // **不允许**在它之前用 `SetWindowPos` 单独改矩形，那会造出"矩形已改、画面未提"的窗口期
    ALPHA.store(fade::FADE_START, Ordering::Relaxed);
    if !render_and_update(hwnd, &rect, &work, shown, scale, fade::FADE_START) {
        diag::log("横幅：提交分层位图失败，本次不显示");
        hide(hwnd);
        return;
    }
    // 圆角区域必须在 ULW **之后**设：ULW 已把窗口改成新尺寸，此时按新尺寸裁才对；
    // 顺序反了会有一帧"旧画面被新区域裁掉"。尺寸没变则区域本来就对，不必重设
    if change == geometry::GeometryChange::Resized {
        set_round_region(hwnd, &rect);
    }
    // 不抢焦点第 2 层（4.4）：显示但不激活
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }

    self_check(hwnd, &work);

    // 停留定时器。定不上就**立刻转淡出**（规格 4.7）：一条永不消失的置顶不透明块比没有反馈
    // 更糟，所以这里不能只记一笔日志就走
    if unsafe { SetTimer(Some(hwnd), TIMER_HOLD, HOLD_MS, None) } == 0 {
        diag::log("横幅：停留定时器定不上，立刻转淡出");
        start_fade(hwnd);
    }
}

/// 目标几何相对窗口当前矩形要提交哪些变化（规格 4.6）。
///
/// 取不到窗口矩形就按"尺寸变了"报：宁可多设一次圆角区域，也不要把该做的动作漏掉。
/// 位置与尺寸为什么必须分开判，见 `geometry::geometry_change` 的文档。
fn window_change(hwnd: HWND, rect: &BannerRect, work: &RECT) -> geometry::GeometryChange {
    let mut current = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut current) }.is_err() {
        return geometry::GeometryChange::Resized;
    }
    geometry::geometry_change(
        [
            current.left,
            current.top,
            current.right - current.left,
            current.bottom - current.top,
        ],
        [
            work.left + rect.x,
            work.top + rect.y,
            rect.width,
            rect.height,
        ],
    )
}

/// 进入淡出（规格 4.3 与 4.7）。
///
/// 淡出定时器也定不上 → **直接隐藏**：定时器是淡出的唯一触发者，定不上就没有任何后续动作，
/// 留着它等于留一块永不消失的置顶不透明块。
fn start_fade(hwnd: HWND) {
    if unsafe { SetTimer(Some(hwnd), TIMER_FADE, FADE_MS, None) } == 0 {
        diag::log("横幅：淡出定时器定不上，直接隐藏");
        hide(hwnd);
    }
}

/// `--bannertest <文本>`：只画一次横幅，5 秒后自动退出（规格 2.2）。
///
/// **必须自杀**：这个模式没有标题栏、收不到 Alt+F4，不退出就会留下一个只能杀进程的常驻
/// 窗口。CLI 模式下没有托盘的消息循环，所以这里自带一个消息泵——横幅的停留与淡出定时器
/// 都靠它才跑得起来。
pub fn run_bannertest(text: &str) -> i32 {
    show(text);
    pump(BANNERTEST_MS)
}

/// 跑消息循环 `ms` 毫秒后返回，返回值为退出码。
///
/// 用**线程定时器**而不是 `sleep` + 轮询：定时器到期会往本线程投递 `WM_TIMER`，`GetMessageW`
/// 立刻醒来，期间不用空转。定不上定时器也无法等（消息泵的唯一唤醒源），按 3 报错——真机上
/// 表现为进程立刻退出，而不是挂着一个既没横幅又没出口的无控制台进程。
fn pump(ms: u32) -> i32 {
    let id = unsafe { SetTimer(None, TIMER_EXIT, ms, None) };
    if id == 0 {
        diag::log("--bannertest：退出定时器定不上，立刻退出");
        return 3;
    }

    let mut msg = MSG::default();
    loop {
        // 返回值 0 是 WM_QUIT、-1 是错误，两者都该收摊
        match unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 {
            0 | -1 => break,
            _ => {}
        }
        if msg.message == WM_TIMER && msg.wParam.0 == id {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unsafe {
        let _ = KillTimer(None, id);
    }
    0
}

// ---- 窗口 ----

/// 懒创建横幅窗口并复用。
fn ensure_window() -> Option<HWND> {
    let existing = BANNER.load(Ordering::Relaxed);
    if existing != 0 {
        return Some(HWND(existing as *mut c_void));
    }

    let instance = match unsafe { GetModuleHandleW(PCWSTR::null()) } {
        Ok(module) => HINSTANCE(module.0),
        Err(e) => {
            diag::log(&format!("横幅：取本进程模块句柄失败: {e}"));
            return None;
        }
    };

    let class = WNDCLASSW {
        style: Default::default(),
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: HICON::default(),
        // 光标留空：横幅带 WS_EX_TOOLWINDOW 且从不激活，指针不会落到它上面
        hCursor: HCURSOR::default(),
        // 空背景刷：完整画面由 UpdateLayeredWindow 提交
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: CLASS_NAME,
    };
    if !*CLASS_READY.get_or_init(|| unsafe { RegisterClassW(&class) } != 0) {
        diag::log("横幅：RegisterClassW 失败，横幅无法创建");
        return None;
    }

    let hwnd = match unsafe {
        CreateWindowExW(
            // 不抢焦点第 1 层（4.4）：NOACTIVATE；点击穿透（4.5）：TRANSPARENT，
            // 没有它就等于用一层看不见的弹窗盖住右下角，全屏游戏那块的点击会被吃掉
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            CLASS_NAME,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            diag::log(&format!("横幅：创建窗口失败: {e}"));
            return None;
        }
    };
    BANNER.store(hwnd.0 as isize, Ordering::Relaxed);
    Some(hwnd)
}

/// 横幅窗口过程。
///
/// 与托盘窗口过程一样整体兜住异常：panic 穿过 `extern "system"` 边界是未定义行为，而这条
/// 路径上没有控制台、只剩日志（坑 #25）。
extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handled = diag::guard("横幅窗口过程", || match msg {
        WM_WINDOWPOSCHANGING => {
            // 不抢焦点第 3 层（4.4）：改几何的那一刻（ShowWindow / SetWindowPos）默认会激活窗口，
            // 这里把 NOACTIVATE 注回 `WINDOWPOS`。缺这一层，前两层会在某个改几何的瞬间一起漏出去
            inject_noactivate(lparam);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_ACTIVATE => {
            // 不抢焦点第 4 层（4.4）：前三层都漏了的话，激活这件事也当没发生
            LRESULT(0)
        }
        WM_TIMER => {
            on_timer(hwnd, wparam.0);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    });
    handled.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
}

/// 往 `WM_WINDOWPOSCHANGING` 的 `WINDOWPOS` 里注入"不激活"（规格 4.4 第 3 层）。
///
/// `lParam` 指向调用方的结构体，就地改 `flags` 即是修改本次几何调整的语义；返给
/// `DefWindowProcW` 让它照常处理。
fn inject_noactivate(lparam: LPARAM) {
    let pos = lparam.0 as *mut WINDOWPOS;
    if pos.is_null() {
        return;
    }
    unsafe {
        (*pos).flags |= SWP_NOACTIVATE;
    }
}

/// 生成一张完整的 32 位 DIB，并原子提交给分层窗口。
fn render_and_update(
    hwnd: HWND,
    rect: &BannerRect,
    work: &RECT,
    text: &str,
    scale: f64,
    alpha: u8,
) -> bool {
    SURFACE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_none_or(|surface| surface.width != rect.width || surface.height != rect.height)
        {
            *slot = create_surface(rect.width, rect.height);
        }
        let Some(surface) = slot.as_mut() else {
            return false;
        };

        surface.x = work.left + rect.x;
        surface.y = work.top + rect.y;
        let client = RECT {
            left: 0,
            top: 0,
            right: rect.width,
            bottom: rect.height,
        };
        let mut units: Vec<u16> = text.encode_utf16().collect();
        draw(surface.dc, &client, &mut units, scale);

        let pixel_count = (rect.width as usize).checked_mul(rect.height as usize);
        let Some(pixel_count) = pixel_count else {
            return false;
        };
        for pixel in unsafe { std::slice::from_raw_parts_mut(surface.pixels, pixel_count) } {
            *pixel |= 0xff00_0000;
        }

        update_layered(hwnd, surface, alpha)
    })
}

fn create_surface(width: i32, height: i32) -> Option<LayeredSurface> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() {
        return None;
    }

    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut raw_pixels = std::ptr::null_mut();
    let bitmap =
        match unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut raw_pixels, None, 0) } {
            Ok(bitmap) => bitmap,
            Err(_) => {
                unsafe {
                    let _ = DeleteDC(dc);
                }
                return None;
            }
        };
    if raw_pixels.is_null() {
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

    Some(LayeredSurface {
        dc,
        bitmap,
        previous,
        pixels: raw_pixels.cast(),
        width,
        height,
        x: 0,
        y: 0,
    })
}

fn update_layered(hwnd: HWND, surface: &LayeredSurface, alpha: u8) -> bool {
    let destination = POINT {
        x: surface.x,
        y: surface.y,
    };
    let source = POINT { x: 0, y: 0 };
    let size = SIZE {
        cx: surface.width,
        cy: surface.height,
    };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: alpha,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    unsafe {
        UpdateLayeredWindow(
            hwnd,
            None,
            Some(&destination),
            Some(&size),
            Some(surface.dc),
            Some(&source),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    }
    .is_ok()
}

fn update_alpha(hwnd: HWND, alpha: u8) -> bool {
    SURFACE.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|surface| update_layered(hwnd, surface, alpha))
    })
}

/// 在 DIB 上铺背景并居中绘制一行文字。
fn draw(hdc: HDC, client: &RECT, text: &mut [u16], scale: f64) {
    // 背景。区域已经裁掉圆角外的那部分，这里直接铺满客户区即可
    let brush = unsafe { CreateSolidBrush(BG_COLOR) };
    unsafe {
        FillRect(hdc, client, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }

    // 字体用完立刻还：GDI 对象不是 GC 管的，缓存它就得在退出、异常、窗口销毁三条路径上
    // 都记得释放，而横幅一次切换只画一两帧，建销成本可以忽略
    let font = create_font(scale);
    let previous = unsafe { SelectObject(hdc, HGDIOBJ(font.0)) };
    let mut rect = *client;
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, TEXT_COLOR);
        // 居中单行 + 超出显示省略号（规格 4.2）：宽度被夹到工作区 3/5 时由它负责收尾
        DrawTextW(
            hdc,
            text,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        SelectObject(hdc, previous);
        let _ = DeleteObject(HGDIOBJ(font.0));
    }
}

/// 定时器回调。
fn on_timer(hwnd: HWND, id: usize) {
    match id {
        TIMER_HOLD => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_HOLD);
            }
            start_fade(hwnd);
        }
        TIMER_FADE => {
            // 只能单调变暗（坑 #19）：`next_fade` 是唯一入口，任何"调回满值"都会让即将消失的
            // 横幅闪回全亮
            let alpha = fade::next_fade(ALPHA.load(Ordering::Relaxed));
            ALPHA.store(alpha, Ordering::Relaxed);
            if alpha == 0 {
                hide(hwnd);
            } else if !update_alpha(hwnd, alpha) {
                // 只记一笔：这一帧没变暗不影响收敛，alpha 照旧递减、到 0 仍会隐藏
                diag::log("横幅：更新分层位图 alpha 失败");
            }
        }
        _ => {}
    }
}

/// 隐藏并复位。alpha 在隐藏**之后**复位：在淡出途中把它调回满值是坑 #19。
fn hide(hwnd: HWND) {
    kill_timers(hwnd);
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    ALPHA.store(fade::FADE_START, Ordering::Relaxed);
}

fn kill_timers(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_HOLD);
        let _ = KillTimer(Some(hwnd), TIMER_FADE);
    }
}

// ---- 度量与绘制辅助 ----

/// 读主显示器工作区。
///
/// **必须检查返回值之后再使用这个矩形**（坑 #3）：失败时它保持全零，拿零值算出来的横幅
/// 会贴到屏幕左上角外，真机上只表现为"切换了但什么都没弹"。
fn work_area() -> Option<RECT> {
    let mut rect = RECT::default();
    let call = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    match call {
        Ok(()) if rect.right > rect.left && rect.bottom > rect.top => Some(rect),
        Ok(()) => {
            diag::log("横幅：SPI_GETWORKAREA 返回了零面积工作区");
            None
        }
        Err(e) => {
            diag::log(&format!("横幅：读工作区失败: {e}"));
            None
        }
    }
}

/// 当前 DPI 缩放（1.0 = 96 DPI）。
///
/// 进程是 PMv2 感知的（见 main.rs），所以这里拿到的是真实 DPI，不需要再乘系统的虚拟化系数。
fn dpi_scale(hwnd: HWND) -> f64 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        1.0
    } else {
        dpi as f64 / 96.0
    }
}

/// 量一行文本的像素尺寸，用的正是绘制时那个字体。
fn measure(text: &str, scale: f64) -> Option<(i32, i32)> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let hdc = unsafe { GetDC(None) };
    if hdc.is_invalid() {
        return None;
    }

    let font = create_font(scale);
    let previous = unsafe { SelectObject(hdc, HGDIOBJ(font.0)) };
    let mut size = SIZE::default();
    let ok = unsafe { GetTextExtentPoint32W(hdc, &units, &mut size) }.as_bool();
    unsafe {
        SelectObject(hdc, previous);
        let _ = DeleteObject(HGDIOBJ(font.0));
        ReleaseDC(None, hdc);
    }

    ok.then_some((size.cx, size.cy))
}

/// 建横幅字体：微软雅黑、粗体、`13 × dpi_scale × 96/72` 像素高、GB2312 字符集、
/// 清晰类型渲染（规格 4.2）。
///
/// 高度取**负值**：负值表示"字身高度"（字符高），正值会被当成含行距的单元格高度，
/// 于是实际文字比量出来的矮一截，垂直居中也就跟着偏。
fn create_font(scale: f64) -> HFONT {
    let height = (FONT_PT * scale * 96.0 / 72.0).round() as i32;
    let face = wide(FACE);
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            GB2312_CHARSET,
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            CLEARTYPE_QUALITY,
            0,
            PCWSTR(face.as_ptr()),
        )
    }
}

/// 按圆角半径裁窗口区域（规格 4.2：用区域裁剪，不用色键抠角）。
///
/// `SetWindowRgn` **成功时区域的所有权转移给窗口**，此后不能再 `DeleteObject`；失败时
/// 所有权留在本进程，得自己释放——两条路径任选一条做错，要么泄漏要么画面残缺。
fn set_round_region(hwnd: HWND, rect: &BannerRect) {
    // 右下角 +1：矩形右/下是开区间，区域不补齐会少一列一行的像素
    let rgn = unsafe {
        CreateRoundRectRgn(
            0,
            0,
            rect.width + 1,
            rect.height + 1,
            rect.radius * 2,
            rect.radius * 2,
        )
    };
    if rgn.is_invalid() {
        diag::log("横幅：CreateRoundRectRgn 失败，本次不带圆角");
        return;
    }
    if unsafe { SetWindowRgn(hwnd, Some(rgn), true) } == 0 {
        diag::log("横幅：SetWindowRgn 失败，本次不带圆角");
        unsafe {
            let _ = DeleteObject(HGDIOBJ(rgn.0));
        }
    }
}

/// 自检（规格 4.8）：横幅实际占的矩形有没有越出工作区。
///
/// 比的必须是**两个来源不同、可能互相矛盾**的量——`GetWindowRect` 问窗口管理器要的实际
/// 矩形，对上 `SPI_GETWORKAREA` 给的工作区。拿刚算出来的 `rect` 自比是假自检：等式按构造
/// 成立，把整段夹取逻辑删掉它也不会响。
fn self_check(hwnd: HWND, work: &RECT) {
    let mut actual = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut actual) }.is_err() {
        diag::log("横幅自检：GetWindowRect 失败，无法确认是否越出工作区");
        return;
    }
    if actual.left < work.left
        || actual.top < work.top
        || actual.right > work.right
        || actual.bottom > work.bottom
    {
        diag::log(&format!(
            "横幅越出工作区: 窗口 ({},{},{},{}) 工作区 ({},{},{},{})",
            actual.left,
            actual.top,
            actual.right,
            actual.bottom,
            work.left,
            work.top,
            work.right,
            work.bottom
        ));
    }
}

/// 转成以 NUL 结尾的宽字符串。
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
