//! 托盘窗口、通知区图标、点击切换与设备菜单。
//!
//! 形态：一个**真实顶层窗口**（坑 #10：仅消息窗口无法成为前台窗口，菜单收不到"点击别处"
//! 的关闭通知，会脱离托盘面板赖在屏幕上）+ 通知区图标 + 消息循环。窗口本身从不显示，
//! 它的唯一作用是接收托盘回调消息并充当菜单的宿主。
//!
//! 事件驱动，无轮询：输入只有通知区回调消息（鼠标）与端点变更通知（`IMMNotificationClient`，
//! 见本文件下半部分）。空闲 CPU 必须是 0%，一切刷新都由事件推着走。
//!
//! 左键语义（切到"上一台"）不是本模块自己的规则，而是 `history` 的三个纯函数 + 括号里的
//! 边界拼出来的：切换本身不可靠时，调试点位毫无意义（M3 必须先于 M6 的原因）。

use crate::audio;
use crate::banner;
use crate::clicks::{self, TrayAction};
use crate::diag;
use crate::history::{self, SwitchOutcome};
use crate::model::Entry;
use crate::naming;
use crate::state_file;
use core::cell::{Cell, RefCell};
use core::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use windows::core::{implement, w, PCWSTR};
use windows::Win32::Foundation::{
    FreeLibrary, GetLastError, COLORREF, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HINSTANCE, HWND,
    LPARAM, LRESULT, PROPERTYKEY, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, CreateFontW,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, FillRect, GetDC, GetObjectW, GetSysColor,
    GetTextExtentPoint32W, GetTextFaceW, GetTextMetricsW, ReleaseDC, SelectObject, SetBkColor,
    SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, COLOR_MENUTEXT, DEFAULT_CHARSET,
    DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_NOCLIP, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE,
    FW_NORMAL, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ, LOGFONTW, OPAQUE, OUT_DEFAULT_PRECIS,
    TEXTMETRICW,
};
use windows::Win32::Media::Audio::{
    eRender, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, DEVICE_STATE,
};
use windows::Win32::System::Console::FreeConsole;
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, LoadLibraryExW, LOAD_LIBRARY_AS_DATAFILE,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{
    ExtractIconExW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow,
    DispatchMessageW, GetIconInfo, GetMessageW, GetSystemMetrics, LoadCursorW, LoadIconW,
    LoadImageW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SystemParametersInfoW, TranslateMessage, CW_USEDEFAULT, HICON, ICONINFO,
    IDC_ARROW, IDI_APPLICATION, IMAGE_FLAGS, IMAGE_ICON, MSG, NONCLIENTMETRICSW, SM_CXSMICON,
    SPI_GETNONCLIENTMETRICS, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOW_EX_STYLE, WM_APP,
    WM_CLOSE, WM_DESTROY, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WNDCLASS_STYLES,
    WS_OVERLAPPEDWINDOW,
};

/// 通知区回调消息（自定义消息都从 `WM_APP` 起算）。
const WM_TRAY: u32 = WM_APP + 1;

/// 端点变更通知转交过来的消息号。
///
/// 回调落在音频服务的工作线程上，**不得在那里碰 UI**（规格 3.4）：它只投递这一条消息，
/// 记账与 tooltip 刷新都回托盘线程做。
const WM_TRAY_DEVICE_CHANGED: u32 = WM_APP + 2;

/// 通知区图标 ID。单实例进程里只有一个图标，取 1 即可。
const ICON_ID: u32 = 1;

/// tooltip 取不到当前默认设备时的固定文案（规格 3.4）。
const FALLBACK_TIP: &str = "切换音频输出";

/// 窗口类名。
const CLASS_NAME: PCWSTR = w!("LiteSoundTrayWindow");

/// 单实例互斥体名（前置一）。
const MUTEX_NAME: PCWSTR = w!("Local\\LiteSoundTray");

/// 从本 exe 里取第几组图标。
///
/// `windows.rc` 只嵌了一组（ID 1），`ExtractIconExW` 的下标 0 读到的就是它。ico 内含
/// 16/24/32/48/256 五帧，系统按请求的大/小尺寸各挑一帧。**只在系统库取不到托盘图标时**
/// 才用到它（规格 3.7 的兜底）：托盘图标表达的是"当前这台是什么设备"，已经不是 exe 图标。
const EXE_ICON_INDEX: i32 = 0;

/// 托盘图案取自**系统自带的图标字体**（规格 3.7）。
///
/// 这两个码位是 `Segoe Fluent Icons` 里的"扬声器带声波"与"耳机"：纯线条、在 16 px 槽位下
/// 仍认得出来，而且是矢量，换任何 DPI 都不会糊。
const ICON_FONT: &str = "Segoe Fluent Icons";
const GLYPH_SPEAKER: u16 = 0xE767;
const GLYPH_HEADPHONE: u16 = 0xE7F6;

/// 菜单里标"这台是蓝牙设备"用的字形（`Segoe Fluent Icons` 的 Bluetooth）。
///
/// 只进菜单勾选位那一格，**不进托盘图标**：托盘槽是 16–24 px，图形 + 角标两件东西塞进去
/// 必然发虚，而"是不是蓝牙"在托盘上远没有"声音是外放还是私人"重要。
const GLYPH_BLUETOOTH: u16 = 0xE702;

/// 托盘图标墨迹与格子四边之间留几像素（物理像素）。
///
/// 实测：本机 150% 下 `SM_CXSMICON = SM_CYSMICON = 24`，即 Shell 给的格子就是 24×24；
/// 旧值按"格子的一成、至少 1 px"内缩 → 24 − 2×3 = **18 px**，图形只占格子的 75%，看着偏小。
/// 收到 1 px → **22 px**（占 91.7%），这已经是格子允许的上限：再大就会被 Shell 裁边/缩放。
/// 格子由 Shell 决定、随 DPI 缩放，**只能**在这个尺寸里把图形撑满，不能提交更大的 HICON。
const ICON_INK_INSET: i32 = 1;

/// 菜单标记的种类。枚举值本身就是**有序标记集合**：`Dot` 在前、`Bluetooth` 在后。
///
/// 旧版把两者各塞进固定列宽的一半，宽度不够时就靠偏移量勉强错开；现在每个 mark 独占
/// 一个显式 cell，顺序由本枚举决定，绘制游标从左到右推进。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mark {
    /// 空集合：这一行没有标记。
    Blank,
    /// 集合 `[Dot]`：当前默认设备的在线绿点（由 `menu` 的动画帧合成）。
    Dot,
    /// 集合 `[Bluetooth]`：蓝牙设备的符号。
    Bluetooth,
    /// 集合 `[Dot, Bluetooth]`：当前那台同时是蓝牙设备。
    Both,
}

impl Mark {
    /// 集合里有没有绿点。
    pub(crate) fn has_dot(self) -> bool {
        matches!(self, Mark::Dot | Mark::Both)
    }

    /// 集合里有没有蓝牙符号。
    pub(crate) fn has_glyph(self) -> bool {
        matches!(self, Mark::Bluetooth | Mark::Both)
    }
}

/// 蓝牙符号墨迹高度占 glyph_cell 的比例（cell 是正方形，高度即短边）。
///
/// 2026-11-02 从 0.80 降到 0.72：当时槽位 29–30 px，符号 24 px 比汉字墨迹还高。
/// 槽位后来收窄到字高 × 0.68，0.72 在 20 px cell 里只剩 14 px 墨迹，真机反馈偏小
/// （2026-11-05）。现取 0.84：100/125/150% 三档墨迹 11/13/17 px，占 cell 85%/81%/85%，
/// 四边仍留 ≥1 px；cell 尺寸与标记列宽不变，菜单宽度增量 0。
const MARK_GLYPH_SHARE: f32 = 0.84;

/// 在线绿点的芯半径、晕外沿（都是槽位短边的比例），以及各自的不透明度。
///
/// 比例来自用户给的参考图：芯 11.5 px、晕 22 px（1.91 倍），晕不透明度 0.22。
/// 换算见 [`dot_pixels`]。
#[cfg(test)]
const CORE_RADIUS_RATIO: f32 = 0.24;
#[cfg(test)]
const HALO_RADIUS_RATIO: f32 = 0.46;
#[cfg(test)]
const CORE_ALPHA: f32 = 0.90;
#[cfg(test)]
const HALO_ALPHA: f32 = 0.22;

/// 取不到字形时的备用图案：`mmres.dll`（控制面板"声音"用的那套库）里的音响与耳机，下标与
/// [`IconKind`] 对齐。
///
/// 字体是 Windows 11 才随系统附带的（Windows 10 上只有另一张脸、码位还对不上），所以回退链
/// 必须留着：字形 → 这两张老图 → exe 内嵌图标。换台机器最多是图案不同，不会没有图标。
const SYSTEM_ICON_DLL: &str = "\\System32\\mmres.dll";
const SYSTEM_ICON_FALLBACK: [i32; 2] = [3010, 3011];

/// 图形该画成什么颜色：深色任务栏画浅色，浅色任务栏画深色。
///
/// 图标字体给的是**单色字形**，颜色得我们自己定；定错了就是"任务栏上什么都看不见"。
/// 托盘跟的是**系统**主题（`SystemUsesLightTheme`），与窗口主题（`AppsUseLightTheme`）是两回事，
/// 所以读系统那一项。读不到按深色算——Windows 11 的默认任务栏就是深色。
fn icon_ink() -> (u8, u8, u8) {
    if system_uses_light_theme() {
        (0x1A, 0x1A, 0x1A)
    } else {
        (0xEC, 0xEC, 0xEC)
    }
}

/// `Personalize` 下那两项的公共读法：值为 1 即"该用浅色"，读不到一律按"不是浅色"。
fn uses_light_theme(value: PCWSTR) -> bool {
    let mut out = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            value,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut out as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && out != 0
}

/// 任务栏（托盘图标跟的那本账）。
fn system_uses_light_theme() -> bool {
    uses_light_theme(w!("SystemUsesLightTheme"))
}

/// 应用窗口与菜单跟的那本账。
fn apps_use_light_theme() -> bool {
    uses_light_theme(w!("AppsUseLightTheme"))
}

/// 诊断用：暴露"应用主题是否浅色"，证明测试进程与生产读的是同一本账。
#[cfg(test)]
pub(crate) fn apps_use_light_theme_for_test() -> bool {
    apps_use_light_theme()
}

/// 让老式 Win32 菜单跟随系统的应用深色模式。
///
/// Win11 的深色设置**不自动**作用在 `#32768` 菜单上，进程要自己声明"本进程允许深色"，否则
/// 菜单永远是浅色。走的是 uxtheme 的两个未公开导出，只能按序号取地址：135 =
/// `SetPreferredAppMode`、136 = `FlushMenuThemes`。取 `AllowDark`（1）而不是 `ForceDark`（2）：
/// 前者跟着 `AppsUseLightTheme` 走，深浅两套都成立；后者会把菜单钉死成深色。
///
/// 取不到地址、调用失败一律不处理：退回浅色菜单，功能一点不受影响。**必须早于任何窗口创建**。
pub fn enable_dark_menu_mode() {
    use windows::core::PCSTR;
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };

    // 只按系统目录解析，不走 dll 搜索路径——避免被同名的旁路 dll 顶掉
    let Ok(lib) =
        (unsafe { LoadLibraryExW(w!("uxtheme.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) })
    else {
        return;
    };
    let Some(set_mode) = (unsafe { GetProcAddress(lib, PCSTR(135 as *const u8)) }) else {
        return;
    };
    let set_mode: unsafe extern "system" fn(i32) -> i32 = unsafe { core::mem::transmute(set_mode) };
    // 1 = AllowDark
    unsafe { set_mode(1) };
    DARK_MENU_ON.store(true, Ordering::Relaxed);
    if let Some(flush) = unsafe { GetProcAddress(lib, PCSTR(136 as *const u8)) } {
        let flush: unsafe extern "system" fn() = unsafe { core::mem::transmute(flush) };
        unsafe { flush() };
    }
}

/// 本进程有没有真的启用"应用深色模式"，由 [`enable_dark_menu_mode`] 在调用成功时置位。
static DARK_MENU_ON: AtomicBool = AtomicBool::new(false);

/// 菜单此刻是不是深色底。
///
/// **判据不能读 `GetSysColor(COLOR_MENU)`。** 真机实测：菜单已经画成 `(44,44,44)` 的深色底，
/// 而同一进程里 `COLOR_MENU` 仍返回 `(240,240,240)`——经典色表根本不跟随应用深色模式。照着
/// 色表判断会连带错两处：动效走浅色分支（把字往底色冲淡，深底上等于不动），蓝牙符号按
/// `COLOR_MENUTEXT` 的黑色画在黑底上直接看不见。
pub(crate) fn menu_is_dark() -> bool {
    DARK_MENU_ON.load(Ordering::Relaxed) && !apps_use_light_theme()
}

/// 托盘图标的两类图案（规格 3.7）：音响与耳机，按当前默认设备的形态因子二选一。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IconKind {
    Speaker = 0,
    Headphone = 1,
}

/// 两类图标帧的句柄，0 表示没取到。
///
/// 放 static 是因为窗口过程是个自由函数，退出时的清理走的是另一条路径，两处要看到同一批
/// 句柄（坑 #21：自取的帧销毁，共享的系统图标不能销毁）。
static ICONS: [AtomicIsize; 2] = [AtomicIsize::new(0), AtomicIsize::new(0)];
static ICONS_OWNED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

/// 蓝牙符号 cell 位图缓存，0 表示还没建。符号只有一种、尺寸随字体固定，
/// 整进程只建一次；退出时由 [`destroy_glyph_bitmap`] 释放。
static GLYPH_BITMAP: AtomicIsize = AtomicIsize::new(0);

/// 上一次看到的默认端点 ID——规格 3.3 里的"参照"。
///
/// 它唯一的用途是区分"外部换了设备"与"自己切的"：自己切完立刻把它推成新设备，于是随后
/// 到达的通知里"参照 == 当前"，记账不会被自己的动作改坏。启动时先填上当时的默认值。
static REFERENCE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// explorer 重启后 shell 广播的 `TaskbarCreated` 消息号（规格 3.6）。
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

thread_local! {
    /// 已注册的通知回调，连同注册时用的那个枚举器（注销要对着同一个实例做）。
    ///
    /// 放 thread_local 而不是 static：COM 接口指针**不是 `Send`**，塞进 static 就只能存成
    /// 裸指针，而那样还得把所有权泄漏掉才能保证音频服务存的那份指针不悬空。注册与注销都
    /// 发生在托盘线程（窗口消息只在创建窗口的线程上派发），thread_local 正好，且不必泄漏。
    static NOTIFY: RefCell<Option<(IMMDeviceEnumerator, IMMNotificationClient)>> =
        const { RefCell::new(None) };
}

fn reference() -> &'static Mutex<Option<String>> {
    REFERENCE.get_or_init(|| Mutex::new(None))
}

/// 设置"参照"。启动时与每次自己切换成功后都要推一次，否则外部变更的判定会错。
fn set_reference(id: Option<&str>) {
    *reference().lock().unwrap_or_else(|e| e.into_inner()) = id.map(str::to_string);
}

fn icon_handle(kind: IconKind) -> HICON {
    HICON(ICONS[kind as usize].load(Ordering::Relaxed) as *mut c_void)
}

/// 形态因子 → 图标类别（规格 3.7）。
///
/// 1 = 扬声器、3 = 耳机、5 = 头戴耳麦、9 = HDMI、10 = 未知：**只有 3 / 5 算耳机**，其余
/// （含读不到）一律音响。二选一的规则下，"读不到"必须落到最常见的那一类，而不是变成一个
/// 第三种状态——否则换台机器、换个驱动就会看到一张谁都不认识的图。
fn icon_kind(form_factor: Option<u32>) -> IconKind {
    match form_factor {
        Some(3) | Some(5) => IconKind::Headphone,
        _ => IconKind::Speaker,
    }
}

/// 托盘模式入口。返回进程退出码。
///
/// 单实例：第二个实例**静默**退出。双击两次是用户正常动作，不是错误，写日志反而会让
/// "diag.txt 有内容就是有事"这条约定失灵。CLI 模式不受此限（规格 2），诊断要能在托盘
/// 已运行时照跑。
pub fn run() -> i32 {
    // 托盘模式不占控制台（规格 2）：控制台子系统保证了 CLI 那边的等待与退出码语义
    // （见 main.rs 头注释），代价是托盘这边要自己把控制台摘掉——双击启动时 Windows 已经
    // 先建好了一个控制台窗口，`FreeConsole` 一脱开它就被销毁；系统里没有控制台时它会失败，
    // 那正是常态，不必记日志。
    let _ = unsafe { FreeConsole() };

    // 句柄要活到进程结束：互斥体对象在最后一个句柄关闭时销毁，所以这个局部变量在函数
    // 返回前一直不能被丢掉（提前 return 的那些分支由另一个实例持有着它）。
    let _mutex = match unsafe { CreateMutexW(None, true, MUTEX_NAME) } {
        Ok(handle) => handle,
        Err(e) => {
            diag::log(&format!("创建单实例互斥体失败: {e}"));
            return 3;
        }
    };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return 0;
    }

    let instance = match unsafe { GetModuleHandleW(PCWSTR::null()) } {
        Ok(module) => HINSTANCE(module.0),
        Err(e) => {
            diag::log(&format!("取本进程模块句柄失败: {e}"));
            return 3;
        }
    };

    store_icons(load_tray_icons());

    let class = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: HICON::default(),
        // 窗口从不显示，光标与类图标只是把结构体填完整；光标的失败不影响任何行为
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: CLASS_NAME,
    };
    // 检查返回值（规格 5.4）：注册失败时窗口必然创建失败，那就是"启动异常"（退出码 3）
    if unsafe { RegisterClassW(&class) } == 0 {
        diag::log("RegisterClassW 失败，托盘窗口无法创建");
        destroy_icons();
        return 3;
    }

    // 真实顶层窗口：父窗口为空、不是 HWND_MESSAGE（坑 #10）。样式给整套常规值，反正不显示。
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
            w!("LiteSound"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance),
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            diag::log(&format!("创建托盘窗口失败: {e}"));
            destroy_icons();
            return 3;
        }
    };

    // tooltip 的内容就是当前默认设备的菜单名（规格 3.4），所以启动时得先读一次默认设备；
    // 参照也一并填成它，之后才分得清"外部换的设备"。
    //
    // 这里会触发音频 COM 的惰性初始化，但**不是**坑 #13 说的"启动阶段急切初始化"：那条
    // 说的是在诊断分支之前抛异常，而 `audio::default_id` 在这条路径上拿不到就返回 None
    //（tooltip 退回固定文案），不抛、不炸。
    let default = audio::default_id();
    set_reference(default.as_deref());

    if !add_icon(hwnd, default.as_deref()) {
        // 不因此退出：explorer 尚未就绪时添加必然失败，而 TaskbarCreated 正是为这种时刻
        // 准备的恢复路径（规格 3.6）。现在就退出，用户会看到"启动了但什么都没有"。
        diag::log("添加托盘图标失败（Shell_NotifyIcon NIM_ADD）");
    }
    register_device_notify(hwnd);

    let code = message_loop();
    clicks::reset();
    destroy_icons();
    destroy_glyph_bitmap();
    destroy_menu_resources();
    code
}

/// 消息循环。
///
/// event-driven，无定时器无轮询——空闲 CPU 必须是 0%（规格 1.2）。
fn message_loop() -> i32 {
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 == -1 {
            diag::log("GetMessageW 返回错误，消息循环中止");
            return 3;
        }
        if ret.0 == 0 {
            // WM_QUIT
            return 0;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// 托盘窗口过程。
///
/// 整段用 [`diag::guard`] 兜住：panic 穿过 `extern "system"` 边界是未定义行为，而且
/// GUI 子系统连控制台都没有，日志里不落一笔就等于什么线索都没留下（坑 #25）。
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handled = diag::guard("托盘窗口过程", || {
        // explorer 重启的广播不是常量消息号，只能问 RegisterWindowMessageW 要；万一要到 0
        //（失败）就当作没这条消息，否则它会把 WM_NULL 一起吞掉。
        let taskbar = taskbar_created_message();
        if taskbar != 0 && msg == taskbar {
            re_add_icon(hwnd);
            return LRESULT(0);
        }

        match msg {
            WM_TRAY => {
                on_tray_message(hwnd, lparam);
                LRESULT(0)
            }
            WM_TRAY_DEVICE_CHANGED => {
                on_device_changed(hwnd);
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                reload_icons(hwnd);
                LRESULT(0)
            }
            WM_TIMER => {
                // 托盘窗上没有自己的定时器了：菜单的动效定时器挂在菜单窗上，随菜单窗销毁。
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            WM_CLOSE => {
                let _ = unsafe { DestroyWindow(hwnd) };
                LRESULT(0)
            }
            WM_DESTROY => {
                // 先注销回调再拆 UI：反过来的话窗口已经没了，回调投递必然失败（只剩日志噪声）
                unregister_device_notify();
                // 趁窗口还活着删图标：窗口销毁后再删，通知区里会留一个要悬停才消失的幽灵图标
                remove_icon(hwnd);
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            // 其余消息（含 WM_CREATE 等）一律交默认处理
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    });

    handled.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
}

/// 通知区回调消息的处理。
unsafe fn on_tray_message(hwnd: HWND, lparam: LPARAM) {
    // **经典布局**：鼠标消息类型在 lParam 低位。设了新式图标版本后这个位置变成坐标、
    // 类型挪到 wParam 高位，点击判定会全错（坑 #9），所以本程序不调 NIM_SETVERSION。
    let mouse = (lparam.0 as u32) & 0xFFFF;
    // 时间源必须是 64 位 tick：32 位 tick 约 24.8 天回绕一次，此后首次点击被吞（坑 #7）
    match clicks::classify(mouse, unsafe { GetTickCount64() }) {
        TrayAction::Toggle => toggle(hwnd),
        TrayAction::Menu => show_menu(hwnd),
        TrayAction::None => {}
    }
}

/// 左键 / 中键：切到"上一台"。
///
/// 没有可切的目标时**退回菜单**而不是什么都不做：左键必须永远有反应，否则用户只会认为
/// 程序坏了。三种退回情形：没有上一台、上一台就是当前默认（规格 3.3 最后一条，此时还要
/// 把它清掉，否则状态文件里会永远留着一条"切到自己"的死记忆）、上一台已经不在 active 里。
fn toggle(hwnd: HWND) {
    let current = audio::default_id();

    let Some(last) = state_file::load() else {
        show_menu(hwnd);
        return;
    };
    if history::same_id(Some(last.as_str()), current.as_deref()) {
        state_file::save(None);
        show_menu(hwnd);
        return;
    }

    let entries = active_entries();
    let Some(target) = entries.iter().find(|e| e.id.eq_ignore_ascii_case(&last)) else {
        show_menu(hwnd);
        return;
    };

    switch_to(target, current.as_deref());
}

/// 右键菜单（规格 3.5）。
///
/// 渲染与交互全在 [`crate::menu`]：单一分层窗，菜单自己画、自己命中。这里只负责
/// "打开前记下快照、关闭后落账"。
fn show_menu(hwnd: HWND) {
    let entries = active_entries();
    let default = audio::default_id();

    // 弹出前先设为前台窗口：菜单窗随后就能直接拿前台（拿到前台才有 ESC 与"点外关闭"）
    let _ = unsafe { SetForegroundWindow(hwnd) };

    // 菜单是模态的但仍会派发托盘消息：菜单开着时再点一次托盘图标会当场改设备，而菜单项
    // 是打开瞬间的快照，松手后会按失效下标派发（坑 #8）。用标志挡掉。
    clicks::set_menu(true);
    let outcome = crate::menu::show(&entries, default.as_deref());
    // 关菜单那一下点击往往在鼠标**抬起**时才变成 WM_TRAY，比 WA_INACTIVE 晚到；
    // 直接放开标志会把"关菜单"误当成"切设备"。并进既有的 250 ms 去抖。
    clicks::close_menu(unsafe { GetTickCount64() });

    match outcome {
        crate::menu::Outcome::Exit => {
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        crate::menu::Outcome::Picked(index) => {
            if let Some(entry) = entries.get(index) {
                // 参照是**打开菜单时**的默认设备：菜单是模态的，期间的账由通知回调对正
                switch_to(entry, default.as_deref());
            }
        }
        crate::menu::Outcome::Cancelled => {}
    }
}

/// 字号相对系统菜单字体的倍数。用户 2026-11-02 第二版定稿 **28 px**（系统 18 px）。
///
/// 150% 下系统菜单字体是 12 个逻辑单位，×1.5556 = 18.67，`lfHeight` 只能取整到 **19**
/// → 设备像素 **28.5 px**。半像素在这里无法消除（-18.67 不是合法 lfHeight），
/// 只能靠 CLEARTYPE_NATURAL_QUALITY 让笔画宽度均匀；实测墨迹高度见交接文档。
const MENU_FONT_SCALE: f32 = 1.5556;
/// 首选字面。**族名必须写全 "Noto Sans SC Medium"**：本机这套 Noto Sans SC 是按字重拆成
/// 独立族注册的——GDI `EnumFontFamiliesExW` 实测 `Noto Sans SC` 族里**只有 400 一档**，
/// `Noto Sans SC Medium` 才是 500。向 `Noto Sans SC` 请求 500 会被族内匹配静默落成 400
/// （diag 实测 `tmWeight=400`），所以真正的 Medium 只能靠族名取。
const MENU_FACE: &str = "Noto Sans SC Medium";
/// 直接请求 500：不再复用那个被落成 400 的 540。
const MENU_FONT_WEIGHT: i32 = 500;
/// 文字基线补偿：YaHei 的字身框中心比行几何中心低 2 px（实测墨迹上留白 19、下留白 15），
/// 把文字矩形整体上移这么多，设备行与"退出"行都用同一套补偿。
pub(crate) const MENU_TEXT_RISE: i32 = 2;
/// 行高 / 标记列高按字高折算。本机 150% 下字高 29 px：行高 29 × 1.897 ≈ **55 px**。
const ROW_HEIGHT_RATIO: f32 = 1.897;
/// 分隔线只占 **3 px**：中间 1 px 线身，上下各 1 px 间隙——它是条目，仍参与布局，
/// 但不再是一整行（用户要"视觉上的一条线"）。仍是 `MF_DISABLED` 条目：鼠标停在上面
/// 不出悬停条、点了也不触发任何动作。
pub(crate) const MENU_SEPARATOR_PX: i32 = 3;
/// "退出"行的高度：55 → **41**。它行内的文字上方留白因此少 7 px；加上分隔线从 6 收到 3
/// （少 3 px），"退出"文字到线的距离一共少 **10 px**（用户要的量）。行本身仍是一个完整
/// 可点条目，只是比设备行矮。
pub(crate) const EXIT_ROW_PX: i32 = 41;
/// 标记列高 = 字高 × 0.68（30 px 字下约 20 px）。旧值 1.3 让槽位 29 px，
/// 绿点 27 px、蓝牙符号 24 px 都比 20 px 的汉字墨迹还大，主次颠倒。
const MARK_HEIGHT_RATIO: f32 = 0.68;
/// 左边距、标记列与名称之间的间距、右边距、圆角半径（物理像素）。
/// 2026-11-02 第三版：三处各缩 5 px（12/14/26 → 7/9/21）。
pub(crate) const MENU_PAD_L: i32 = 7;
pub(crate) const MENU_GAP: i32 = 9;
/// 相邻两个标记 cell 之间的显式间隙。旧布局用"两段拼成列宽"，缝由列宽公式偶然给出；
/// 现在点 cell 与符号 cell 之间固定留这么多，重叠不再可能。
pub(crate) const MARK_GAP: i32 = 4;
pub(crate) const MENU_PAD_R: i32 = 21;
pub(crate) const MENU_RADIUS: i32 = 12;

thread_local! {
    /// 菜单字体：`(句柄, 字高 px)`，0 表示还没建。
    static MENU_FONT: Cell<(isize, i32)> = const { Cell::new((0, 0)) };
}

/// 一套菜单配色。
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) bg: u32,
    pub(crate) hover: u32,
    pub(crate) text: u32,
    pub(crate) dim: u32,
    pub(crate) separator: u32,
}

pub(crate) fn menu_palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bg: 0x002C_2C2C,
            hover: 0x003A_3A3A,
            text: 0x00FF_FFFF,
            dim: 0x00B4_B4B4,
            separator: 0x003E_3E3E,
        }
    } else {
        Palette {
            bg: 0x00F0_F0F0,
            hover: 0x00E2_E2E2,
            text: 0x0000_0000,
            dim: 0x005A_5A5A,
            separator: 0x00C8_C8C8,
        }
    }
}

/// 菜单字体：从系统菜单字体派生、按 [`MENU_FONT_SCALE`] 放大。整进程只建一次。
pub(crate) fn menu_font() -> (HFONT, i32) {
    let (raw, px) = MENU_FONT.with(Cell::get);
    if raw != 0 {
        return (HFONT(raw as *mut c_void), px);
    }
    let mut metrics: NONCLIENTMETRICSW = unsafe { core::mem::zeroed() };
    metrics.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            Some(&mut metrics as *mut NONCLIENTMETRICSW as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    // lfHeight 是**逻辑单位**：系统菜单字体返回的 -18 是当前 DPI 下的物理值，
    // 直接乘倍数再交给 CreateFontIndirectW 会被再放大一次（150% 下 -22 渲染成 33 px）。
    // 先换回逻辑单位、按倍数缩放，再折回物理值去算行高。
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() } as f32;
    let dpi_scale = if dpi > 0.0 { dpi / 96.0 } else { 1.0 };
    // 先取整到**逻辑单位**再乘倍数：系统字体是 -12 个逻辑单位，1.667 倍正好 -20，
    // 150% 下是整数 30 px。留着 -12.0 的小数去乘，会在某些 DPI 上得到半个物理像素。
    let logical = (metrics.lfMenuFont.lfHeight as f32 / dpi_scale).round();
    let mut logfont: LOGFONTW = metrics.lfMenuFont;
    let scaled = (logical * MENU_FONT_SCALE).round() as i32;
    logfont.lfHeight = scaled;
    logfont.lfWeight = MENU_FONT_WEIGHT;
    set_face(&mut logfont, MENU_FACE);
    // 2026-11-03 同参数样张实测（同一 GDI 路径、同一字号/字重、同一底色）：
    // `CLEARTYPE_NATURAL_QUALITY` 会在灰字行的笔画边缘生成 **1400–1700 个次像素彩边像素**
    // （用户眼里的"白点/彩点"，一行 700×46 的样张里就有这么多），灰度抗锯齿后为 **0**；
    // 当前行（黑底白字）在扫光峰值帧的孤立亮点也从 77 降到 53。**白点的根因是渲染方式，不是
    // 字体**——换成灰度抗锯齿就够了，不必换字体。
    // 更根本的原因是分层窗：`WS_EX_LAYERED` + `UpdateLayeredWindow` 交的是逐像素 alpha，
    // DWM 会把三个通道各自与未知壁纸合成，ClearType 的 3×1 相位补偿一定变成彩边（物理限制）。
    logfont.lfQuality = ANTIALIASED_QUALITY;
    let mut font = unsafe { CreateFontIndirectW(&logfont) };
    let (mut face, mut weight) = realized_font(font);
    if face != MENU_FACE {
        // 这台机器没装 Noto Sans SC：退回家用的系统菜单字面，行为回到 2026-11-03 之前。
        // 不静默降级——diag 里必须留一条。
        unsafe {
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
        logfont.lfFaceName = metrics.lfMenuFont.lfFaceName;
        font = unsafe { CreateFontIndirectW(&logfont) };
        let (fallback_face, fallback_weight) = realized_font(font);
        face = fallback_face;
        weight = fallback_weight;
        diag::log(&format!(
            "菜单字体回退：本机没有 {MENU_FACE}，实际 face={face} tmWeight={weight}"
        ));
    }
    diag::log(&format!(
        "菜单字体：请求 face={MENU_FACE} lfWeight={MENU_FONT_WEIGHT} lfHeight={}；\
         实际 face={face} tmWeight={weight}",
        logfont.lfHeight
    ));
    let px = (scaled as f32 * dpi_scale).abs().round() as i32;
    MENU_FONT.with(|cell| cell.set((font.0 as isize, px)));
    (font, px)
}

/// 写 `LOGFONT.lfFaceName`（`LF_FACESIZE` = 32 个 UTF-16 单元，超长截断）。
fn set_face(logfont: &mut LOGFONTW, name: &str) {
    let units: Vec<u16> = name.encode_utf16().take(31).collect();
    logfont.lfFaceName = [0u16; 32];
    logfont.lfFaceName[..units.len()].copy_from_slice(&units);
}

/// 真实落到的字面与字重。
///
/// `GetObject` 只能回读**请求值**（要 500 就报 500），所以证明"到底挑中了哪个字面"必须走 DC：
/// `GetTextFaceW` 给实际字面名，`GetTextMetricsW.tmWeight` 给实际字重。
/// 这就是 540 → 400 那次"以为改了字重其实没改"的教训。
fn realized_font(font: HFONT) -> (String, i32) {
    let dc = unsafe { GetDC(None) };
    let previous = unsafe { SelectObject(dc, HGDIOBJ(font.0)) };
    let mut face = [0u16; 64];
    let got = unsafe { GetTextFaceW(dc, Some(&mut face)) };
    let mut metrics: TEXTMETRICW = unsafe { core::mem::zeroed() };
    let met_ok = unsafe { GetTextMetricsW(dc, &mut metrics) }.as_bool();
    unsafe {
        SelectObject(dc, previous);
        ReleaseDC(None, dc);
    }
    let name = if got > 0 {
        let end = face.iter().position(|u| *u == 0).unwrap_or(face.len());
        String::from_utf16_lossy(&face[..end])
    } else {
        String::from("<取不到>")
    };
    (name, if met_ok { metrics.tmWeight } else { -1 })
}

pub(crate) fn menu_row_height(px: i32) -> i32 {
    (px as f32 * ROW_HEIGHT_RATIO).round().max(1.0) as i32
}

pub(crate) fn text_width(text: &[u16], font: HFONT) -> i32 {
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return 0;
    }
    let previous = unsafe { SelectObject(screen, HGDIOBJ(font.0)) };
    let mut size = SIZE::default();
    // 结尾那个 0 不算进宽度
    let body = &text[..text.len().saturating_sub(1)];
    let ok = unsafe { GetTextExtentPoint32W(screen, body, &mut size) }.as_bool();
    unsafe {
        SelectObject(screen, previous);
        let _ = ReleaseDC(None, screen);
    }
    if ok {
        size.cx
    } else {
        0
    }
}

pub(crate) fn fill_rect(hdc: HDC, rect: &RECT, color: u32) {
    let brush = unsafe { CreateSolidBrush(COLORREF(color)) };
    unsafe {
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

/// 把一张已经按 cell 尺寸做好的位图画到指定 cell（左上角 + 宽高都由调用方给）。
///
/// 这里**不做任何相对列宽的定位**：cell 的坐标来自绘制游标，函数只负责贴图。
pub(crate) fn draw_mark(hdc: HDC, left: i32, top: i32, width: i32, height: i32, bitmap: HBITMAP) {
    if width <= 0 || height <= 0 {
        return;
    }
    let mem = unsafe { CreateCompatibleDC(Some(hdc)) };
    if mem.is_invalid() {
        return;
    }
    let previous = unsafe { SelectObject(mem, HGDIOBJ(bitmap.0)) };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    unsafe {
        let _ = AlphaBlend(
            hdc, left, top, width, height, mem, 0, 0, width, height, blend,
        );
        SelectObject(mem, previous);
        let _ = DeleteDC(mem);
    }
}

/// 切换 + 记账 + 横幅反馈。
///
/// **记账只在确认成功之后**（规格 6.5）：失败时把"上一台"写成当前设备，就把左键锁死了。
/// 成功时离开的是 `before`，新的默认是 `id`（`set_default` 已回读确认过），按 3.3
/// "外部换了设备"那一行把"上一台"改成我们离开的那台——下一次左键才能切回去。
///
/// 点当前那台只报设备名、**不再重开菜单**（规格 4.1）。菜单项已不再置灰，所以点当前那台
/// 就是这条分支的常见走法，图标与横幅都要照实给反馈。
fn switch_to(target: &Entry, before: Option<&str>) -> SwitchOutcome {
    let outcome = audio::set_default(&target.id);
    match outcome {
        SwitchOutcome::Switched => {
            let next = history::next_last(state_file::load().as_deref(), before, Some(&target.id));
            if let Some(next) = next {
                state_file::save(next.as_deref());
            }
            // 自己切完立刻把参照推成新设备：随后到达的通知里"参照 == 当前"，记账不会被自己的
            // 动作改坏（规格 3.3 第二行）
            set_reference(Some(&target.id));
            banner::show(&target.name);
        }
        SwitchOutcome::AlreadyCurrent => {
            // 只报设备名：用户要的是"点到了"这个反馈。写"已经是: xxx"除了多一层解释，
            // 还把这行和真正切换成功的那行分成了两种观感（规格 4.1 的旧文案已按用户要求改）
            banner::show(&target.name);
        }
        SwitchOutcome::Failed => banner::show(&format!("切换失败: {}", target.name)),
    }
    outcome
}

// ---- 设备变更通知（规格 3.4）与 explorer 重启（规格 3.6） ----

/// 端点变更通知回调。
///
/// 这是本项目唯一需要**实现**（而非调用）的 COM 接口：音频服务注册时只存裸指针，既不做
/// 接口查询也不报错——写错的话回调会落到别的槽位上，**永远不响且毫无提示**（坑 #1）。
/// 所以用 crate 的 `#[implement]` 生成 `IUnknown` 与 `QueryInterface`，不手写 vtable。
///
/// 回调落在音频服务的工作线程上，**这里一个 UI 操作都不做**：只投一条消息，记账与 tooltip
/// 刷新都回到托盘线程（`on_device_changed`）。
#[implement(IMMNotificationClient)]
struct EndpointNotify {
    /// 托盘窗口句柄。存 `isize` 而不是 `HWND`：句柄类型不是 `Send`，而回调会跨线程来。
    hwnd: isize,
}

impl EndpointNotify {
    fn post_changed(&self) {
        if let Err(e) = unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut c_void)),
                WM_TRAY_DEVICE_CHANGED,
                WPARAM(0),
                LPARAM(0),
            )
        } {
            // 正常收尾时窗口可能已经没了，这属于预期内；真要排查"事件没反应"时它是第一条线索
            diag::log(&format!("投递设备变更消息失败: {e}"));
        }
    }
}

impl IMMNotificationClient_Impl for EndpointNotify_Impl {
    fn OnDeviceStateChanged(
        &self,
        _id: &PCWSTR,
        _state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.post_changed();
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        // 这三个回调不带流信息，只能一并刷：设备上/下线会改变菜单内容，也可能把当前默认
        // 那台拔掉（3.4 的"只处理渲染流"能落在下面的默认设备变更上，那里 flow 是可得的）
        self.post_changed();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        self.post_changed();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        _id: &PCWSTR,
    ) -> windows::core::Result<()> {
        // 只处理渲染流（规格 3.4）。role 不过滤：tooltip 跟的是 eConsole 那台，而重复刷新
        // 是幂等的（`refresh_notify` 每次都重读当前默认）
        if flow == eRender {
            self.post_changed();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        // 属性变更一律忽略（规格 3.4）：它对应的是改名/改图标这类事，与 tooltip 无关
        Ok(())
    }
}

/// 注册通知回调。
///
/// **注册结果必须记日志**（规格 3.4）：它失败时的表现是"tooltip 不跟随"，没有日志就只能靠猜。
/// 失败不退出——托盘本身（点击、菜单）仍然可用。
fn register_device_notify(hwnd: HWND) -> bool {
    let Some(enumerator) = audio::enumerator() else {
        diag::log("拿不到端点枚举器，设备变更通知未注册");
        return false;
    };
    let client: IMMNotificationClient = EndpointNotify {
        hwnd: hwnd.0 as isize,
    }
    .into();

    match unsafe { enumerator.RegisterEndpointNotificationCallback(&client) } {
        Ok(()) => {
            NOTIFY.with(|slot| *slot.borrow_mut() = Some((enumerator, client)));
            true
        }
        Err(e) => {
            diag::log(&format!("注册设备变更通知失败: {e}"));
            false
        }
    }
}

/// 注销通知回调（规格 3.4 要求注册与注销的返回值都进日志）。
///
/// `take()` 出来的那一对在本函数结束时被 drop：回调对象的引用计数因此归零并真正释放，
/// 不留一个"注册着但已经没人管"的对象。
fn unregister_device_notify() {
    let taken = NOTIFY.with(|slot| slot.borrow_mut().take());
    if let Some((enumerator, client)) = taken {
        if let Err(e) = unsafe { enumerator.UnregisterEndpointNotificationCallback(&client) } {
            diag::log(&format!("注销设备变更通知失败: {e}"));
        }
    }
}

/// 收到设备变更：**先**对正"上一台"记账，**再**刷新通知区（tooltip 与图标，规格 3.4 / 3.7）。
fn on_device_changed(hwnd: HWND) {
    align_reference();
    refresh_notify(hwnd);
}

/// 重新取一遍托盘图标并刷新通知区。
///
/// 图标字体给的是单色字形，颜色得跟着任务栏深浅走（[`icon_ink`]）；用户切换浅色/深色主题时
/// 系统会广播 `WM_SETTINGCHANGE`，这里重取一次，否则会留着一张与背景同色的"看不见的图标"。
fn reload_icons(hwnd: HWND) {
    destroy_icons();
    store_icons(load_tray_icons());
    refresh_notify(hwnd);
}

/// 按 3.3 对正"上一台"，并把"参照"推到当前默认。
///
/// 两步的顺序不能反：必须先用**旧的**参照判定"这是不是外部换的设备"，判定完再推参照。
fn align_reference() {
    let current = audio::default_id();
    let mut reference = reference().lock().unwrap_or_else(|e| e.into_inner());
    let next = history::next_last(
        state_file::load().as_deref(),
        reference.as_deref(),
        current.as_deref(),
    );
    if let Some(next) = next {
        state_file::save(next.as_deref());
    }
    *reference = current;
}

/// explorer 重启后 shell 会广播 `TaskbarCreated`（规格 3.6）。
fn taskbar_created_message() -> u32 {
    *TASKBAR_CREATED.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
}

/// 重新添加托盘图标。
///
/// explorer 重启会清掉**所有**托盘图标：不重加的话图标消失而进程还活着，再双击会被单实例
/// 互斥静默挡掉（规格 3.6 点名的那个现象）。tooltip 在这里一并重算——它和图标是同一次
/// 调用写进通知区的，3.6 的"重新添加图标 + 刷新 tooltip"就是这一件事。
fn re_add_icon(hwnd: HWND) {
    if !add_icon(hwnd, audio::default_id().as_deref()) {
        diag::log("explorer 重启后重新添加托盘图标失败");
    }
}

/// tooltip 文案：当前默认设备的**菜单名**截断 60 字符，取不到时用固定文案（规格 3.4）。
///
/// 刻意不套 [`naming::menu_text`]：那一步还会把 `&` 翻倍，而那是菜单助记符的规矩，
/// tooltip 里显示成 `B&&O` 就成了错字。
fn tip_for(default: Option<&str>) -> String {
    let entries = active_entries();
    entries
        .iter()
        .find(|e| history::same_id(default, Some(e.id.as_str())))
        .map(|e| naming::truncate_chars(&e.name, naming::MAX_CHARS).to_string())
        .unwrap_or_else(|| FALLBACK_TIP.to_string())
}

/// 刷新通知区内容：tooltip 与图标（规格 3.4 / 3.7）。
///
/// 两样都要跟着默认设备走：换到耳机，tooltip 是耳机名、图标也得是耳机。一次 `NIM_MODIFY`
/// 写完整块数据。
fn refresh_notify(hwnd: HWND) {
    let default = audio::default_id();
    let data = icon_data(hwnd, icon_for_default(), Some(&tip_for(default.as_deref())));
    if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
        diag::log("刷新 tooltip / 图标失败（Shell_NotifyIcon NIM_MODIFY）");
    }
}

/// 默认设备该用哪张图标：读它的形态因子，二选一（规格 3.7）。
fn icon_for_default() -> HICON {
    let form_factor = audio::default_form_factor();
    if form_factor.is_none() {
        // "读不到"与"读到扬声器"都会走音响图标，但成因完全不同，留一笔才查得下去
        diag::log("读不到默认设备的形态因子，托盘图标按音响处理");
    }
    icon_handle(icon_kind(form_factor))
}

/// 把文本写进定长 UTF-16 缓冲，超出部分截掉。
///
/// 缓冲是零初始化的，NUL 结尾天然成立；`saturating_sub(1)` 就是给它留的那一格。
fn copy_wide(buffer: &mut [u16], text: &str) {
    let room = buffer.len().saturating_sub(1);
    for (index, unit) in text.encode_utf16().take(room).enumerate() {
        buffer[index] = unit;
    }
}

/// 菜单/tooltip 用的设备切片：**只取 active**（规格 6.4），并整批编号。
///
/// 与 `cli::run_menu` 用同一套切片与编号，否则真机菜单和 `--menu` 输出会对不上（验收项 5）。
fn active_entries() -> Vec<Entry> {
    let mut entries = audio::enumerate(audio::ACTIVE);
    naming::number(&mut entries);
    entries
}

/// 取两张托盘图标（音响 / 耳机，规格 3.7）：图案优先取自本机现有资源。
///
/// 返回值是 `[(句柄, 是否归我们所有); 2]`，下标与 [`IconKind`] 对齐。回退链见
/// [`SYSTEM_ICON_FALLBACK`]：字形取不到就换 mmres.dll，再取不到才退到 exe 内嵌图标。
fn load_tray_icons() -> [(HICON, bool); 2] {
    let slot = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let ink = icon_ink();
    let mut icons = [(HICON::default(), false); 2];
    for (index, glyph) in [GLYPH_SPEAKER, GLYPH_HEADPHONE].into_iter().enumerate() {
        icons[index] = glyph_icon(glyph, slot, ink)
            .map(|icon| (icon, true))
            .or_else(|| {
                diag::log(&format!(
                    "图标字体里取不到第 {index} 类图案，退回 {}",
                    SYSTEM_ICON_DLL
                ));
                load_system_icon(SYSTEM_ICON_FALLBACK[index]).map(|icon| (icon, true))
            })
            .unwrap_or_else(|| {
                diag::log(&format!(
                    "第 {index} 类图案在图标字体与 {} 里都没有，退回 exe 内嵌图标",
                    SYSTEM_ICON_DLL
                ));
                load_exe_icon()
            });
    }
    icons
}

/// 把图标字体里的一个字形做成托盘图标（规格 3.7）。
///
/// 先按槽位字号渲一遍量出墨迹，再按墨迹比例重渲一次：字形在字身框里留白多少，各码位并不
/// 一样，直接按槽位字号画出来的大小全凭运气（16 槽下差几个像素就是"小到看不清"）。
fn glyph_icon(glyph: u16, slot: i32, ink: (u8, u8, u8)) -> Option<HICON> {
    if slot <= 0 {
        return None;
    }
    let extent = ink_extent(&glyph_coverage(glyph, slot, slot)?, slot, slot)?;
    // 目标墨迹 = 格子去掉两侧 1 px 留白，图形撑满格子（见 ICON_INK_INSET）
    let target = (slot - 2 * ICON_INK_INSET).max(1);
    let scale = target as f32 / extent.0.max(extent.1) as f32;
    let height = (slot as f32 * scale).round() as i32;
    let coverage = glyph_coverage(glyph, height.max(1), slot)?;

    let blob = icon_image(&coverage, slot, slot, ink)?;
    // 交给系统自己的 ico 解析器：32 位位图的 alpha 会被原样保留
    unsafe { CreateIconFromResourceEx(&blob, true, 0x0003_0000, 0, 0, IMAGE_FLAGS(0)) }.ok()
}

/// 把字形渲染成"覆盖度"图：白字画在黑底上，灰度值即抗锯齿覆盖率（0..=255）。
fn glyph_coverage(glyph: u16, font_height: i32, slot: i32) -> Option<Vec<u8>> {
    if font_height <= 0 || slot <= 0 {
        return None;
    }
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return None;
    }
    let dc = unsafe { CreateCompatibleDC(Some(screen)) };
    let _ = unsafe { ReleaseDC(None, screen) };
    if dc.is_invalid() {
        return None;
    }

    let coverage = draw_glyph(dc, glyph, font_height, slot);
    let _ = unsafe { DeleteDC(dc) };
    coverage
}

/// 在 DC 上画一个字形并读回覆盖度。画布与字体都在这里申请、在这里释放。
fn draw_glyph(dc: HDC, glyph: u16, font_height: i32, slot: i32) -> Option<Vec<u8>> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: slot,
            biHeight: -slot, // 负高度 = 自顶向下，与 `ink_extent` 的行序一致
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap =
        unsafe { CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;

    let name = wide(ICON_FONT);
    // 字号取字符高度；抗锯齿用灰阶（CLEARTYPE 会给字形边缘带上彩色，在黑底上是脏边）
    let font = unsafe {
        CreateFontW(
            -font_height,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR(name.as_ptr()),
        )
    };

    let previous_bitmap = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
    let previous_font = unsafe { SelectObject(dc, HGDIOBJ(font.0)) };
    // 字体没装时 CreateFontW 不报错、GDI 换一张脸照画，结果是一串豆腐块：先问实际用的是哪张脸
    let coverage = (font_is_ours(dc) && !bits.is_null())
        .then(|| draw_glyph_pixels(dc, glyph, slot, bits))
        .flatten();

    unsafe {
        SelectObject(dc, previous_font);
        SelectObject(dc, previous_bitmap);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
    }
    coverage
}

/// DC 里实际生效的是不是我们要的那张字形字体。
fn font_is_ours(dc: HDC) -> bool {
    let mut face = [0u16; 64];
    if unsafe { GetTextFaceW(dc, Some(&mut face)) } <= 0 {
        return false;
    }
    let end = face
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(face.len());
    String::from_utf16_lossy(&face[..end]).eq_ignore_ascii_case(ICON_FONT)
}

/// 画一个字形并读回覆盖度图。
fn draw_glyph_pixels(dc: HDC, glyph: u16, slot: i32, bits: *mut c_void) -> Option<Vec<u8>> {
    unsafe {
        // 黑底白字：DrawText 会把背景涂成 bk 色，覆盖率就落在灰度上
        SetBkMode(dc, OPAQUE);
        SetBkColor(dc, COLORREF(0));
        SetTextColor(dc, COLORREF(0x00FF_FFFF));
    }
    let mut text = [glyph, 0];
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: slot,
        bottom: slot,
    };
    unsafe {
        DrawTextW(
            dc,
            &mut text[..1],
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP,
        );
    }

    // 白字黑底三通道相等，取最大值即覆盖率
    let raw = unsafe { std::slice::from_raw_parts(bits as *const u8, (slot * slot * 4) as usize) };
    Some(
        raw.as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[0].max(pixel[1]).max(pixel[2]))
            .collect(),
    )
}

/// 覆盖度图里墨迹的外接矩形 `(宽, 高)`；整张空白返回 `None`。
///
/// 纯函数，供 [`glyph_icon`] 定字号用，也便于单测。
fn ink_extent(coverage: &[u8], width: i32, height: i32) -> Option<(i32, i32)> {
    let (left, top, right, bottom) = ink_box(coverage, width, height)?;
    Some((right - left + 1, bottom - top + 1))
}

/// 覆盖度图里墨迹的边界盒 `(左, 上, 右, 下)`，闭区间；整张空白返回 `None`。
///
/// 只量宽高会把字形自带的左右边距丢掉：`DrawTextW` 走 `DT_LEFT`，墨迹并不落在画布第 0 列，
/// 要把符号右对齐到名称前，得知道墨迹右缘在画布里的真实列号。
fn ink_box(coverage: &[u8], width: i32, height: i32) -> Option<(i32, i32, i32, i32)> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let (mut left, mut top) = (width, height);
    let (mut right, mut bottom) = (-1, -1);
    for y in 0..height {
        for x in 0..width {
            if coverage.get((y * width + x) as usize).copied().unwrap_or(0) == 0 {
                continue;
            }
            left = left.min(x);
            right = right.max(x);
            top = top.min(y);
            bottom = bottom.max(y);
        }
    }
    (right >= 0).then_some((left, top, right, bottom))
}

/// 把覆盖度图拼成一段图标资源字节流：`BITMAPINFOHEADER` + 颜色位图 + AND 掩码。
///
/// 纯函数，便于单测（头字段、行序、预乘不变式）。两个格式细节：位图**自底向上**，行序要倒
/// 过来；AND 掩码行按 4 字节对齐、**全 0**，表示"透不透明由 alpha 说了算"。
fn icon_image(coverage: &[u8], width: i32, height: i32, ink: (u8, u8, u8)) -> Option<Vec<u8>> {
    if width <= 0 || height <= 0 || coverage.len() < (width * height) as usize {
        return None;
    }
    let (red, green, blue) = ink;
    let color_len = (width * height * 4) as usize;
    let mask_row = (width + 31) / 32 * 4;
    let mut blob = vec![0u8; 40 + color_len + (mask_row * height) as usize];

    blob[0..4].copy_from_slice(&40u32.to_le_bytes());
    blob[4..8].copy_from_slice(&width.to_le_bytes());
    // 高度是两倍：上半是颜色位图，下半是 AND 掩码
    blob[8..12].copy_from_slice(&(height * 2).to_le_bytes());
    blob[12..14].copy_from_slice(&1u16.to_le_bytes());
    blob[14..16].copy_from_slice(&32u16.to_le_bytes());
    // 压缩方式 BI_RGB = 0，其余字段留 0

    for y in 0..height {
        let source = (y * width) as usize;
        let target = 40 + ((height - 1 - y) * width * 4) as usize;
        for x in 0..width {
            let alpha = coverage[source + x as usize] as u32;
            let offset = target + (x * 4) as usize;
            // 预乘 alpha：颜色通道先乘上自己的透明度，否则半透明边缘会泛白
            blob[offset] = (blue as u32 * alpha / 255) as u8;
            blob[offset + 1] = (green as u32 * alpha / 255) as u8;
            blob[offset + 2] = (red as u32 * alpha / 255) as u8;
            blob[offset + 3] = alpha as u8;
        }
    }
    Some(blob)
}

/// 从系统音频资源库按资源 id 取一帧（规格 3.7）。
///
/// 库以 `LOAD_LIBRARY_AS_DATAFILE` 方式映射：不跑 `DllMain`、不加载依赖，只当资源容器用。
/// 帧取完立刻放掉映射——`LoadImageW` 没给 `LR_SHARED`，交回来的是我们自己的拷贝，映射消失
/// 不影响它，同时也不会给进程留下一个驻留的模块。
fn load_system_icon(resource_id: i32) -> Option<HICON> {
    let root = std::env::var("SystemRoot").ok()?;
    let path = wide(&format!("{root}{SYSTEM_ICON_DLL}"));
    let module =
        unsafe { LoadLibraryExW(PCWSTR(path.as_ptr()), None, LOAD_LIBRARY_AS_DATAFILE) }.ok()?;

    // 槽位边长就是 `SM_CXSMICON`（规格 3.7）：交给我们自己选尺寸，比大/小两帧二选一更准
    let slot = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let loaded = unsafe {
        LoadImageW(
            Some(HINSTANCE(module.0)),
            PCWSTR(resource_id as usize as *const u16),
            IMAGE_ICON,
            slot,
            slot,
            IMAGE_FLAGS(0),
        )
    };
    let _ = unsafe { FreeLibrary(module) };

    match loaded {
        Ok(handle) if handle.0 as isize != 0 => Some(HICON(handle.0)),
        _ => None,
    }
}

/// 取 exe 内嵌图标：`ExtractIconExW(本 exe, 0)` 的两帧里挑一帧，两帧都拿不到才退回系统
/// 默认应用图标（规格 3.7 的兜底链）。
///
/// 返回值第二项是"这个句柄归不归我们"：自取的帧退出时销毁；退回的系统默认应用图标是
/// **共享**的，销毁它会牵连其它进程（坑 #21）。
fn load_exe_icon() -> (HICON, bool) {
    let mut large = HICON::default();
    let mut small = HICON::default();
    let exe = std::env::current_exe().unwrap_or_default();
    let path = wide(&exe.to_string_lossy());
    let extracted = unsafe {
        ExtractIconExW(
            PCWSTR(path.as_ptr()),
            EXE_ICON_INDEX,
            Some(&mut large),
            Some(&mut small),
            1,
        )
    };

    let has_large = !large.is_invalid();
    let has_small = !small.is_invalid();
    if extracted > 0 && (has_large || has_small) {
        // 16 槽用 16 帧才不发虚（规格 3.7）：按 SM_CXSMICON 选帧——它**就是**通知区那个槽位
        // 的边长。想用的那帧缺失时退回另一帧，而不是整个退回系统图标：有真图标总比没有强。
        let use_small = has_small && (!has_large || small_fits_slot(small));
        let (keep, drop) = if use_small {
            (small, large)
        } else {
            (large, small)
        };
        // 未被选中的那帧立刻销毁：留着就是一份白占的 GDI 资源
        if !drop.is_invalid() {
            let _ = unsafe { DestroyIcon(drop) };
        }
        return (keep, true);
    }

    // 共享句柄：只记日志，绝不销毁
    diag::log(
        "本 exe 里两帧图标都取不到（检查 build.rs 是否嵌入了 windows.rc），退回系统默认应用图标",
    );
    let fallback = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
    (fallback, false)
}

/// 把两张图标登记进 static（窗口过程与退出清理都要看到同一批句柄）。
fn store_icons(icons: [(HICON, bool); 2]) {
    for (index, (icon, owned)) in icons.into_iter().enumerate() {
        ICONS[index].store(icon.0 as isize, Ordering::Relaxed);
        ICONS_OWNED[index].store(owned, Ordering::Relaxed);
    }
}

/// 小帧够不够填通知区那个槽位。
///
/// 判据是**量出小帧自己的像素宽度**，而不是拿一个门限去猜：实测 150% 缩放下
/// `SM_CXSMICON` 是 24，`ExtractIconExW` 从我们的 ico 里按槽位挑出的正是 24 帧——那才是为
/// 这个槽位画的那一帧，用大帧去缩会糊一点；而 100% 缩放下两者都是 16，也就是规格点名的
/// "16 槽用 16 帧"。量不到宽度（罕见）时退回"槽位不大于 16 就用小帧"，即 96 DPI 下的老判据。
fn small_fits_slot(icon: HICON) -> bool {
    let slot = unsafe { GetSystemMetrics(SM_CXSMICON) };
    match icon_width(icon) {
        Some(width) => width >= slot,
        None => slot <= 16,
    }
}

/// 图标帧的像素宽度；拿不到返回 `None`。
fn icon_width(icon: HICON) -> Option<i32> {
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(icon, &mut info) }.ok()?;

    let mut bitmap = BITMAP::default();
    let read = unsafe {
        GetObjectW(
            HGDIOBJ(info.hbmColor.0),
            size_of::<BITMAP>() as i32,
            Some(&mut bitmap as *mut BITMAP as *mut c_void),
        )
    };
    // GetIconInfo 会为 mask 与 color 各复制一份位图，用完必须还（否则每次枚举都漏 GDI 句柄）
    let _ = unsafe { DeleteObject(HGDIOBJ(info.hbmMask.0)) };
    let _ = unsafe { DeleteObject(HGDIOBJ(info.hbmColor.0)) };

    (read > 0).then_some(bitmap.bmWidth)
}

/// 销毁**自取的**图标句柄。共享的系统图标在这里被跳过（坑 #21）。
fn destroy_icons() {
    for (index, slot) in ICONS.iter().enumerate() {
        let raw = slot.swap(0, Ordering::Relaxed);
        if raw == 0 || !ICONS_OWNED[index].swap(false, Ordering::Relaxed) {
            continue;
        }
        let _ = unsafe { DestroyIcon(HICON(raw as *mut c_void)) };
    }
}

/// 标记 cell 的边长（由菜单字高折算，30 px 字下 20 px）。
///
/// 只描述高度：绿点 cell 的宽度由 `menu` 里的环外沿决定（比槽高更大），蓝牙符号 cell
/// 则是正方形，边长与这里一致。`mark_geometry` 仍保留旧列宽公式，供旧测试与诊断使用。
pub(crate) fn mark_height(px: i32) -> i32 {
    (px as f32 * MARK_HEIGHT_RATIO).round().max(1.0) as i32
}

/// 旧版标记列尺寸：`2 × 槽长 + 4`。新布局不再使用这个列宽（它装不下绿点环），
/// 但旧单元测试和诊断输出仍按它复现历史行为。
pub(crate) fn mark_geometry() -> Option<(i32, i32)> {
    let (_, px) = menu_font();
    if px <= 0 {
        return None;
    }
    let height = mark_height(px);
    Some((height * 2 + 4, height))
}

/// 取蓝牙符号在自己 cell 里的位图（懒建、整进程只建一次）。
///
/// cell 是正方形，边长等于 [`mark_geometry`] 的槽高。位图不按 cell 的尺寸做会被
/// AlphaBlend 拉变形；句柄归缓存，退出时由 [`destroy_glyph_bitmap`] 释放。
pub(crate) fn glyph_bitmap(width: i32, height: i32) -> Option<HBITMAP> {
    let cached = GLYPH_BITMAP.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(HBITMAP(cached as *mut c_void));
    }

    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // 负高度 = 自顶向下：行序与 `dot_pixels` 一致，省掉一次翻转
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap = match unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }
    {
        Ok(bitmap) if !bits.is_null() => bitmap,
        _ => return None,
    };

    let Some(pixels) = glyph_cell_pixels(width, height, MARK_GLYPH_SHARE) else {
        // 符号画不出来（字体里没有这个字形）：把刚建的位图还回去，不留空白句柄
        let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        return None;
    };
    // AlphaBlend 的 `AC_SRC_ALPHA` **要求预乘 alpha**（文档原文：通道值必须乘以 alpha），
    // `glyph_cell_pixels` 出来的已经是预乘，直接拷进 DIB。上一版在这里多转了一道直 alpha：
    // GDI 仍按预乘相加，255 的半透明边缘 `255 + dst×(1−a)` 在 8 位里回绕成暗色，
    // 合成阶段再把亮度低于底色的像素压成底色，整枚符号只剩中心一列（2026-11-05 真机反馈）。
    unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len()) };
    GLYPH_BITMAP.store(bitmap.0 as isize, Ordering::Relaxed);
    Some(bitmap)
}

/// 诊断用：按生产路径建一张**新的**符号位图，并把 DIB 里的原始字节读回来。
///
/// 与 [`glyph_bitmap`] 的唯一差别是不进缓存，用来确认"交给 AlphaBlend 的到底是谁"
/// （DIB 里必须是预乘 BGRA）。
#[cfg(test)]
pub(crate) fn glyph_bitmap_bits_for_test(width: i32, height: i32) -> Option<Vec<u8>> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
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
        unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
    let pixels = glyph_cell_pixels(width, height, MARK_GLYPH_SHARE)?;
    unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len()) };
    let out = unsafe { std::slice::from_raw_parts(bits as *const u8, pixels.len()) }.to_vec();
    let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
    Some(out)
}

/// 蓝牙符号 cell 的像素：`width × height`、预乘 alpha。
///
/// 墨迹按 `share` 撑满槽位后，按**墨迹盒**在 cell 里居中——不在列中心、
/// 也不贴列右缘，落位完全由自己这个 cell 决定。
fn glyph_cell_pixels(width: i32, height: i32, share: f32) -> Option<Vec<u8>> {
    let slot = width.min(height);
    if slot <= 0 {
        return None;
    }
    let coverage = bluetooth_coverage(slot, share)?;
    let (left, top, right, bottom) = ink_box(&coverage, slot, slot)?;
    let ink = ink_canvas(&coverage, slot, menu_ink())?;
    let mut canvas = vec![0u8; (width * height * 4) as usize];
    let dx = (width - (right - left + 1)) / 2 - left;
    let dy = (height - (bottom - top + 1)) / 2 - top;
    overlay(&mut canvas, width, &ink, slot, slot, dx, dy);
    Some(canvas)
}

/// 诊断用：直接拿到符号 cell 的预乘像素（不经过 GDI 位图缓存）。
#[cfg(test)]
pub(crate) fn glyph_cell_pixels_for_test(width: i32, height: i32, share: f32) -> Option<Vec<u8>> {
    glyph_cell_pixels(width, height, share)
}

/// 诊断用：当前生效的符号墨迹占比。
#[cfg(test)]
pub(crate) fn mark_share_for_test() -> f32 {
    MARK_GLYPH_SHARE
}

/// 释放菜单字体与背景刷（各一份，整进程只建一次）。
fn destroy_menu_resources() {
    MENU_FONT.with(|cell| {
        let raw = cell.replace((0, 0)).0;
        if raw != 0 {
            let _ = unsafe { DeleteObject(HGDIOBJ(raw as *mut c_void)) };
        }
    });
}

/// 释放蓝牙符号 cell 位图（`CreateDIBSection` 交出来的位图必须自己还）。
fn destroy_glyph_bitmap() {
    let raw = GLYPH_BITMAP.swap(0, Ordering::Relaxed);
    if raw != 0 {
        let _ = unsafe { DeleteObject(HGDIOBJ(raw as *mut c_void)) };
    }
}

/// **旧布局**的整列标记像素：32 位 BGRA、预乘 alpha。仅供单元测试复现历史行为。
///
/// 生产路径已经改为"每个 mark 一个 cell"（[`glyph_cell_pixels`] + `menu` 里的动画点），
/// 这里的"单点居中 / 符号贴列尾"落位不再参与菜单绘制。
#[cfg(test)]
pub(crate) fn legacy_mark_pixels(kind: Mark, width: i32, height: i32) -> Option<Vec<u8>> {
    mark_pixels(kind, width, height)
}

/// **旧布局**的整列标记像素：32 位 BGRA、预乘 alpha。仅供单元测试复现历史行为。
#[cfg(test)]
fn mark_pixels(kind: Mark, width: i32, height: i32) -> Option<Vec<u8>> {
    let segment = (width / 2).min(height);
    if segment <= 0 {
        return None;
    }
    let mut canvas = vec![0u8; (width * height * 4) as usize];

    // 绿点的落位分两种：
    // - 只有点（多数设备行）：落在**整列正中**，名称从列后起排，视觉上不会一边挤一边空；
    // - 点和蓝牙符号同时在场（当前的蓝牙设备）：点退到左段中心，符号占右段。否则两颗
    //   墨迹在 44 px 的窄列里会撞在一起（旧布局靠列宽大才勉强没碰上）。
    if matches!(kind, Mark::Dot | Mark::Both) {
        let dot = dot_pixels(height, height);
        let dx = if matches!(kind, Mark::Both) {
            0
        } else {
            (width - height) / 2
        };
        overlay(&mut canvas, width, &dot, height, height, dx, 0);
    }
    if matches!(kind, Mark::Bluetooth | Mark::Both) {
        let coverage = bluetooth_coverage(segment, MARK_GLYPH_SHARE)?;
        let glyph = ink_canvas(&coverage, segment, menu_ink())?;
        // 按**墨迹**右缘贴列尾，不按 sprite 右缘：字形走 `DT_LEFT`，自带一截右边距，
        // 拿画布对齐会把这截边距垫在符号和名称之间，"紧贴名称"就成了空话。
        let ink = ink_box(&coverage, segment, segment)?;
        overlay(
            &mut canvas,
            width,
            &glyph,
            segment,
            segment,
            width - 1 - ink.2,
            (height - segment) / 2,
        );
    }
    Some(canvas)
}

/// 把蓝牙字形渲成 `slot×slot` 的覆盖率图，墨迹按 `share` 比例撑满槽位。
///
/// 先渲一遍量墨迹、再按目标比例重渲（与托盘图标同一手法）：字形在字身框里的留白各码位
/// 不同，直接按槽位字号画，出来的大小全凭运气。
fn bluetooth_coverage(slot: i32, share: f32) -> Option<Vec<u8>> {
    let extent = ink_extent(&glyph_coverage(GLYPH_BLUETOOTH, slot, slot)?, slot, slot)?;
    let target = (slot as f32 * share).round() as i32;
    let height =
        (slot as f32 * target as f32 / (extent.0.max(extent.1)).max(1) as f32).round() as i32;
    glyph_coverage(GLYPH_BLUETOOTH, height.max(1), slot)
}

/// 覆盖率图 → 预乘 alpha 的墨色画布（`slot×slot`，自顶向下）。
fn ink_canvas(coverage: &[u8], slot: i32, ink: (u8, u8, u8)) -> Option<Vec<u8>> {
    if slot <= 0 || coverage.len() < (slot * slot) as usize {
        return None;
    }
    let (red, green, blue) = ink;
    Some(
        coverage
            .iter()
            .flat_map(|&cover| {
                let alpha = cover as u32;
                [
                    (blue as u32 * alpha / 255) as u8,
                    (green as u32 * alpha / 255) as u8,
                    (red as u32 * alpha / 255) as u8,
                    alpha as u8,
                ]
            })
            .collect(),
    )
}

/// 把 `sprite`（`sw`×`sh` 预乘 BGRA）以 src-over 合成进 `canvas` 的 `(dx, dy)`。
///
/// 两侧都是**预乘** alpha，所以合成式就是 `dst = src + dst×(1−src.a)`，不需要先除回去
/// （除再乘会在半透明边缘留下暗边）。越界部分裁掉：槽位尺寸是系统给的奇数时，居中偏移
/// 会让 sprite 伸出一侧一两个像素，裁掉比画到行首去要好解释。
fn overlay(
    canvas: &mut [u8],
    canvas_width: i32,
    sprite: &[u8],
    sw: i32,
    sh: i32,
    dx: i32,
    dy: i32,
) {
    for y in 0..sh {
        let target_y = dy + y;
        if target_y < 0 || target_y >= canvas_height(canvas, canvas_width) {
            continue;
        }
        for x in 0..sw {
            let target_x = dx + x;
            if target_x < 0 || target_x >= canvas_width {
                continue;
            }
            let source = ((y * sw + x) * 4) as usize;
            let target = ((target_y * canvas_width + target_x) * 4) as usize;
            let src_a = sprite[source + 3] as u32;
            if src_a == 0 {
                continue;
            }
            if src_a == 255 {
                canvas[target..target + 4].copy_from_slice(&sprite[source..source + 4]);
                continue;
            }
            let keep = 255 - src_a;
            for channel in 0..4 {
                let mixed = sprite[source + channel] as u32
                    + (canvas[target + channel] as u32 * keep) / 255;
                canvas[target + channel] = mixed.min(255) as u8;
            }
        }
    }
}

/// 预乘画布的行数，由字节长度和行宽反推——省得每个调用点多传一个高度参数。
fn canvas_height(canvas: &[u8], canvas_width: i32) -> i32 {
    if canvas_width <= 0 {
        return 0;
    }
    (canvas.len() / (canvas_width as usize * 4)) as i32
}

/// 菜单标记该画成什么颜色：跟着系统给菜单的文字色走。
///
/// 托盘图标跟的是**任务栏**主题（[`icon_ink`]），菜单跟的是**菜单**自己的配色，两者读的键
/// 不一样。取 `COLOR_MENUTEXT` 而不是自己按深浅猜：系统画什么底我们就出什么字，深色菜单
/// 白字、浅色菜单黑字一次到位。
fn menu_ink() -> (u8, u8, u8) {
    if menu_is_dark() {
        // 深色菜单上 `COLOR_MENUTEXT` 仍是黑，照它画就是"黑字画在黑底上"。真机实测深色菜单
        // 的字色是纯白，直接取那个值。
        (0xFF, 0xFF, 0xFF)
    } else {
        let color = unsafe { GetSysColor(COLOR_MENUTEXT) };
        // `COLORREF` 是 0x00BBGGRR
        (
            ((color >> 16) & 0xFF) as u8,
            ((color >> 8) & 0xFF) as u8,
            (color & 0xFF) as u8,
        )
    }
}

/// "在线绿点"的像素：32 位 BGRA、**预乘 alpha**。
///
/// 造型 = 实心芯 + 一圈半透明的晕（2026-10-30 按用户给的参考图改）。参考图（271×322 截图）
/// 的径向读数：芯半径 ≈ 11.5 px、纯色 `(98,208,93)`；晕从 12 px 起、外沿 ≈ 22 px，白底上的
/// 颜色 ≈ `(215,245,220)`，按 RGB 三通道反推约等于 0.22 的不透明度，最后约 2 px 渐隐。
/// 换算到本机 23 px 槽位：芯 0.24 × 短边、晕 0.46 × 短边（比例 1.92，参考图是 1.91），
/// 外沿给槽位留约 1 px。
///
/// 芯与晕**同色**、各自内部都不做渐变：渐变会把它变成一颗球或一团光晕，而这是状态点。
/// 芯的不透明度固定 0.9，让底下的菜单底色透出来一点，也就是要的"半透明"。
///
/// 纯函数：给尺寸就出像素，便于单测（预乘不变式 + 芯扁平 + 晕是薄薄一圈 + 四角全透）。
/// 当前行实际用的是 `menu` 里的动画帧，这份静态点只留在旧测试里。
#[cfg(test)]
fn dot_pixels(width: i32, height: i32) -> Vec<u8> {
    let mut pixels = vec![0u8; (width.max(0) * height.max(0) * 4) as usize];
    let center_x = (width as f32 - 1.0) * 0.5;
    let center_y = (height as f32 - 1.0) * 0.5;
    let short = width.min(height) as f32;
    let core_radius = short * CORE_RADIUS_RATIO;
    let halo_radius = short * HALO_RADIUS_RATIO;
    // 抗锯齿过渡带固定 1 px：够把圆周磨平，又不会把轮廓泡开
    let feather = 1.0f32;
    // 在线绿（取自聊天软件那颗点的 #12D97F）
    let (red, green, blue) = (0x12 as f32, 0xD9 as f32, 0x7F as f32);

    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = (x as f32 - center_x, y as f32 - center_y);
            let distance = (dx * dx + dy * dy).sqrt();
            let core = ((core_radius - distance) / feather).clamp(0.0, 1.0);
            let halo = ((halo_radius - distance) / (feather * 1.6)).clamp(0.0, 1.0);
            // 晕在芯底下，按 over 合成：a = a芯 + a晕 × (1 − a芯)
            let core_alpha = CORE_ALPHA * core;
            let alpha = core_alpha + HALO_ALPHA * halo * (1.0 - core_alpha);
            if alpha <= 0.0 {
                continue;
            }

            let offset = ((y * width + x) * 4) as usize;
            pixels[offset] = (blue * alpha) as u8;
            pixels[offset + 1] = (green * alpha) as u8;
            pixels[offset + 2] = (red * alpha) as u8;
            pixels[offset + 3] = (alpha * 255.0) as u8;
        }
    }
    pixels
}

/// 通知区图标的数据块。
///
/// `cbSize` 给整个结构体的大小（Vista 之后的合法写法）。**不设**新式图标版本（坑 #9）。
/// `tip` 为 `None` 时不带 `NIF_TIP`——删除图标用不到它。
fn icon_data(hwnd: HWND, icon: HICON, tip: Option<&str>) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    if let Some(tip) = tip {
        data.uFlags |= NIF_TIP;
        copy_wide(&mut data.szTip, tip);
    }
    data
}

/// 添加通知区图标：tooltip 与图标都按当前默认设备算（规格 3.4 / 3.7）。
fn add_icon(hwnd: HWND, default: Option<&str>) -> bool {
    let data = icon_data(hwnd, icon_for_default(), Some(&tip_for(default)));
    unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool()
}

fn remove_icon(hwnd: HWND) {
    let data = icon_data(hwnd, HICON::default(), None);
    if !unsafe { Shell_NotifyIconW(NIM_DELETE, &data) }.as_bool() {
        // 典型成因是 explorer 刚重启过（图标本来就没了），记一笔才能一眼看出是这个
        diag::log("删除托盘图标失败（Shell_NotifyIcon NIM_DELETE）");
    }
}

/// 转成以 NUL 结尾的宽字符串。
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_kind_treats_only_headphone_and_headset_as_headphone() {
        // 3 = 耳机、5 = 头戴耳麦（规格 3.7 的二选一）
        assert_eq!(icon_kind(Some(3)), IconKind::Headphone);
        assert_eq!(icon_kind(Some(5)), IconKind::Headphone);
    }

    #[test]
    fn icon_kind_falls_back_to_speaker_for_others_and_missing() {
        // 1 = 扬声器、9 = HDMI、10 = 未知；读不到也不能变成第三种状态
        for value in [Some(0), Some(1), Some(2), Some(4), Some(9), Some(10), None] {
            assert_eq!(icon_kind(value), IconKind::Speaker, "{value:?}");
        }
    }

    #[test]
    fn blank_mark_is_fully_transparent() {
        let pixels = mark_pixels(Mark::Blank, 32, 16).unwrap();
        assert_eq!(pixels.len(), 32 * 16 * 4);
        assert!(pixels.iter().all(|&byte| byte == 0));
    }

    /// 标记列里两颗墨迹的落位：单点时居中、点 + 符号时各占一段、符号右贴列尾。
    ///
    /// 断言全部按**墨迹盒**量，不按画布盒——这条规则的意义就是"按看得见的像素算"，按画布
    /// 算的话字形自带那截右边距会把缝吃掉而测试照样绿。
    #[test]
    fn mark_inks_sit_centered_and_clear_of_each_other() {
        // 产物真实尺寸（150% DPI、Per-Monitor V2、30 px 字）：段长 20 × 2 + 4 = 44、高 20
        let (width, height) = (44, 20);
        let alpha_box = |pixels: &[u8]| -> Option<(i32, i32, i32, i32)> {
            let (mut left, mut top) = (width, height);
            let (mut right, mut bottom) = (-1, -1);
            for y in 0..height {
                for x in 0..width {
                    if pixels[((y * width + x) * 4 + 3) as usize] == 0 {
                        continue;
                    }
                    left = left.min(x);
                    right = right.max(x);
                    top = top.min(y);
                    bottom = bottom.max(y);
                }
            }
            (right >= 0).then_some((left, top, right, bottom))
        };

        let dot = alpha_box(&mark_pixels(Mark::Dot, width, height).unwrap()).unwrap();
        assert_eq!(
            dot.0,
            width - 1 - dot.2,
            "单点不在标记列正中：左留白 {}，右留白 {}",
            dot.0,
            width - 1 - dot.2
        );

        // 本机没装那张图标字体时符号拿不到值，跳过而不是判红
        let Some(bitmap) = mark_pixels(Mark::Bluetooth, width, height) else {
            return;
        };
        let bluetooth = alpha_box(&bitmap).unwrap();
        assert_eq!(bluetooth.2, width - 1, "蓝牙墨迹右缘没贴到列尾");

        // 两颗同时在场时：点退到左段，符号仍在右段。按**Both 自己的**落位量缝，
        // 不能拿"单点居中"的坐标去和符号比——单点那行本来就没有符号。
        let both = alpha_box(&mark_pixels(Mark::Both, width, height).unwrap()).unwrap();
        assert!(
            both.0 < dot.0,
            "Both 里的点没有退到左段：Both 左缘 {}，单点左缘 {}",
            both.0,
            dot.0
        );
        assert_eq!(both.2, bluetooth.2, "Both 里符号没有贴列尾");
        // Both 的墨迹盒是两颗的并集，用半段中线反推点右缘更直接：点画在 0..height 里
        let dot_right = both.0 + (height - 1);
        assert!(
            dot_right + 3 < bluetooth.0,
            "两颗之间只剩 {} px 缝，会看着连在一起",
            bluetooth.0 - dot_right - 1
        );
    }

    #[test]
    fn dot_pixels_are_sized_and_premultiplied() {
        let (width, height) = (16, 16);
        let pixels = dot_pixels(width, height);
        assert_eq!(pixels.len(), (width * height * 4) as usize);

        // 预乘不变式：任何像素的颜色通道都不大于它的 alpha（否则边缘会泛灰）
        for pixel in pixels.as_chunks::<4>().0 {
            let alpha = pixel[3];
            for channel in &pixel[..3] {
                assert!(*channel <= alpha);
            }
        }
    }

    #[test]
    fn dot_pixels_are_a_small_flat_disc() {
        let (width, height) = (16, 16);
        let pixels = dot_pixels(width, height);
        let alpha_at = |x: usize, y: usize| pixels[(y * 16 + x) * 4 + 3];

        // 芯是 0.9、晕是 0.22；圆心处两块叠起来 = 0.9 + 0.22 × 0.1 ≈ 0.922
        let peak = alpha_at(8, 8);
        assert!(
            (233..=237).contains(&peak),
            "峰值应约 0.92×255，实际 {peak}"
        );
        // 芯内部各处同值：有明暗渐变才会像一颗球
        assert_eq!(alpha_at(8, 7), peak, "上半球不该更亮");
        assert_eq!(alpha_at(7, 8), peak, "左半球不该更暗");

        // 芯半径 = 短边 24% = 3.84；晕外沿 = 46% = 7.36，再往外留 1.6 px 渐隐
        // 距圆心 5.5 处只有晕：0.22 × 255 ≈ 56，且上下左右同值（晕是圆的）
        let halo = alpha_at(13, 8);
        assert!((54..=58).contains(&halo), "晕应约 0.22×255，实际 {halo}");
        assert_eq!(alpha_at(8, 13), halo, "晕不是圆的");
        assert!(halo * 3 < peak, "晕太亮，看着会像一团光而不是一圈边");

        // 晕外沿之外全透：这也是一颗点，不是一块底色
        assert_eq!(alpha_at(15, 8), 0);
        assert_eq!(alpha_at(8, 15), 0);
        for (x, y) in [(0, 0), (15, 0), (0, 15), (15, 15)] {
            assert_eq!(alpha_at(x, y), 0, "({x},{y})");
        }
    }

    #[test]
    fn ink_extent_finds_the_drawn_box() {
        // 5 宽 3 高的画布，墨迹落在 (1,1)-(2,2)
        let mut canvas = vec![0u8; 15];
        canvas[6] = 255;
        canvas[7] = 200;
        canvas[11] = 128;
        canvas[12] = 255;
        assert_eq!(ink_extent(&canvas, 5, 3), Some((2, 2)));
    }

    #[test]
    fn ink_extent_is_none_for_a_blank_canvas() {
        assert_eq!(ink_extent(&[0u8; 15], 5, 3), None);
        // 尺寸非法时同样不给值，免得调用方拿它去除
        assert_eq!(ink_extent(&[], 0, 3), None);
    }

    #[test]
    fn icon_image_is_a_bottom_up_32bpp_dib_with_a_zeroed_mask() {
        let (width, height) = (2, 2);
        let coverage = vec![255u8, 255, 255, 255];
        let blob = icon_image(&coverage, width, height, (0xEC, 0xEC, 0xEC)).expect("拼出字节流");

        // 头 40 字节是 BITMAPINFOHEADER；高度翻倍 = 颜色位图 + AND 掩码
        assert_eq!(u32::from_le_bytes(blob[0..4].try_into().unwrap()), 40);
        assert_eq!(i32::from_le_bytes(blob[4..8].try_into().unwrap()), width);
        assert_eq!(
            i32::from_le_bytes(blob[8..12].try_into().unwrap()),
            height * 2
        );
        assert_eq!(u16::from_le_bytes(blob[12..14].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(blob[14..16].try_into().unwrap()), 32);

        // 颜色位图全不透明：alpha 满值，颜色通道已被预乘（不变式：不大于 alpha）
        let color = &blob[40..40 + (width * height * 4) as usize];
        for pixel in color.as_chunks::<4>().0 {
            assert_eq!(pixel[3], 255);
            for channel in &pixel[..3] {
                assert!(*channel <= pixel[3]);
            }
        }

        // 掩码行按 4 字节对齐且全 0：透不透明由 alpha 说了算
        let mask_row = ((width + 31) / 32 * 4) as usize;
        assert_eq!(
            blob.len(),
            40 + (width * height * 4) as usize + mask_row * height as usize
        );
        assert!(blob[40 + (width * height * 4) as usize..]
            .iter()
            .all(|b| *b == 0));
    }

    #[test]
    fn icon_image_reverses_rows_because_dibs_are_bottom_up() {
        // 上排全透明、下排全不透明
        let coverage = vec![0u8, 0, 255, 255];
        let blob = icon_image(&coverage, 2, 2, (0, 0, 0)).expect("拼出字节流");

        // 位图首行应是**下**排（自底向上），即不透明的那排
        assert_eq!(blob[40 + 3], 255, "首行应是不透明的下排");
        assert_eq!(blob[40 + 8 + 3], 0, "次行应是透明的上排");
    }

    #[test]
    fn ink_canvas_premultiplies_the_coverage() {
        let ink = (0x12, 0xD9, 0x7F);
        let canvas = ink_canvas(&[0, 128, 255, 255], 2, ink).expect("四格画布");

        // 全透那格四个通道都得是 0：留着颜色只留 alpha，半透边缘会泛出一圈暗边
        assert_eq!(&canvas[0..4], &[0, 0, 0, 0]);
        // 半覆盖：每个通道都按自己的 alpha 缩过，顺序是 BGRA
        assert_eq!(&canvas[4..8], &[0x3F, 0x6C, 0x09, 128]);
        // 全覆盖：就是墨色本身
        assert_eq!(
            &canvas[8..16],
            &[0x7F, 0xD9, 0x12, 255, 0x7F, 0xD9, 0x12, 255]
        );
    }

    #[test]
    fn ink_canvas_refuses_a_coverage_that_is_too_short() {
        // 宁可不出图，也不要读到画布外面
        assert!(ink_canvas(&[255, 255, 255], 2, (0, 0, 0)).is_none());
        assert!(ink_canvas(&[255, 255, 255, 255], 0, (0, 0, 0)).is_none());
    }

    #[test]
    fn overlay_is_src_over_on_premultiplied_pixels() {
        // 目标：左边一格半透的红，右边一格全空
        let mut canvas = vec![10u8, 0, 0, 100, 0, 0, 0, 0];
        overlay(&mut canvas, 2, &[0, 0, 200, 200], 1, 1, 0, 0);

        // keep = 255 − 200：每通道 dst = src + dst×keep
        assert!(canvas[0] > 0 && canvas[0] < 10, "红通道被部分盖住但没盖死");
        assert_eq!(canvas[1], 0);
        assert_eq!(canvas[2], 200);
        assert!(canvas[3] > 100 && canvas[3] < 255, "alpha 只增不减");
        assert_eq!(
            &canvas[4..8],
            &[0, 0, 0, 0],
            "没贴到的那一格一个字节都不许动"
        );
    }

    #[test]
    fn overlay_clips_a_sprite_that_does_not_fit() {
        // 槽位宽度是奇数时居中偏移会让源伸出一列：伸出部分裁掉，绝不绕回画布行首
        let mut canvas = vec![0u8; 2 * 2 * 4];
        overlay(&mut canvas, 2, &[9u8; 2 * 2 * 4], 2, 2, 1, 0);

        assert_eq!(canvas[4..8].to_vec(), vec![9, 9, 9, 9], "(1,0) 被覆盖");
        assert_eq!(canvas[12..16].to_vec(), vec![9, 9, 9, 9], "(1,1) 被覆盖");
        assert!(canvas[0..4].iter().all(|&b| b == 0), "(0,0) 该保持空白");
        assert!(
            canvas[8..12].iter().all(|&b| b == 0),
            "左列第二行不该收到右列的数据"
        );
    }

    #[test]
    fn overlay_short_circuits_fully_transparent_and_fully_opaque() {
        let mut canvas = vec![7u8, 7, 7, 7, 8, 8, 8, 8];
        overlay(&mut canvas, 2, &[0, 0, 0, 0], 1, 1, 0, 0);
        assert_eq!(&canvas[0..4], &[7, 7, 7, 7], "全透的源不动目标");

        overlay(&mut canvas, 2, &[1, 2, 3, 255], 1, 1, 1, 0);
        assert_eq!(&canvas[4..8], &[1, 2, 3, 255], "全不透明的源整格换掉");
    }

    /// 诊断：符号 cell 的像素在三种 DPI 下是否真有墨迹、落点是否居中。
    ///
    /// `cargo test --bin litesound -- --ignored dump_glyph_cell --nocapture`
    #[test]
    #[ignore = "诊断：打印符号 cell 的墨迹 bbox"]
    fn dump_glyph_cell() {
        for share in [0.72, 0.84, 0.95, 1.0] {
            println!("---- share={share} ----");
            for (dpi, slot) in [(100, 13), (125, 16), (150, 20)] {
                let Some(pixels) = glyph_cell_pixels(slot, slot, share) else {
                    println!("{dpi}% slot={slot}: glyph_cell_pixels 返回 None");
                    continue;
                };
                let (mut left, mut top) = (slot, slot);
                let (mut right, mut bottom, mut count) = (-1, -1, 0u32);
                for y in 0..slot {
                    for x in 0..slot {
                        if pixels[((y * slot + x) * 4 + 3) as usize] == 0 {
                            continue;
                        }
                        count += 1;
                        left = left.min(x);
                        top = top.min(y);
                        right = right.max(x);
                        bottom = bottom.max(y);
                    }
                }
                let ink_w = right - left + 1;
                let ink_h = bottom - top + 1;
                println!(
                    "{dpi}% slot={slot}: cell=0..{} 墨迹 {count} px，bbox=[{left},{top}]..[{right},{bottom}] \
                     （{ink_w}×{ink_h}，占 cell {:.0}%×{:.0}%），留白 左{left} 右{} 上{top} 下{}，越界={}",
                    slot - 1,
                    ink_w as f32 / slot as f32 * 100.0,
                    ink_h as f32 / slot as f32 * 100.0,
                    slot - 1 - right,
                    slot - 1 - bottom,
                    left < 0 || top < 0 || right >= slot || bottom >= slot,
                );
            }
        }
    }

    /// 把四种标记按真实菜单槽尺寸、菜单底色合成后写成 BMP，供人工目视。
    ///
    /// `#[ignore]`：它要 GDI、要写字盘，产物是一次性的观察材料，不该进回归。跑法
    /// `cargo test -- --ignored dump_marks_for_eyeballing`，图落在 `%TEMP%\litesound-marks.bmp`。
    #[test]
    #[ignore]
    fn dump_marks_for_eyeballing() {
        use std::io::Write;

        let (width, height) = mark_geometry().expect("拿不到菜单槽尺寸");
        // 底色写死浅色模式的菜单灰：这张图只看形状与分段，墨色由 menu_ink 现取系统色
        let bg = [0xF0u32, 0xF0, 0xF0];

        let kinds = [Mark::Blank, Mark::Dot, Mark::Bluetooth, Mark::Both];
        let count = kinds.len() as i32;
        let gap = 2;
        let canvas_width = width * count + gap * (count - 1);
        let mut rgb = vec![0u8; (canvas_width * height * 3) as usize];

        for (column, kind) in kinds.iter().enumerate() {
            let pixels = mark_pixels(*kind, width, height).expect("标记画不出来");
            let origin = column as i32 * (width + gap);
            for y in 0..height {
                for x in 0..width {
                    let source = ((y * width + x) * 4) as usize;
                    let alpha = pixels[source + 3] as u32;
                    let target = ((y * canvas_width + origin + x) * 3) as usize;
                    // 源是预乘的，直接加回底色即可，除回 alpha 只会带进舍入噪声
                    for channel in 0..3 {
                        let over = pixels[source + channel] as u32;
                        let kept = bg[channel] * (255 - alpha) / 255;
                        rgb[target + channel] = (over + kept).min(255) as u8;
                    }
                }
            }
        }

        // 24 位 BMP：正高度 = 自底向上，所以要按行翻一次；行宽补零到 4 的倍数
        let row_bytes = (canvas_width as usize * 3).div_ceil(4) * 4;
        let size = 54 + row_bytes * height as usize;
        let mut file = Vec::with_capacity(size);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(size as u32).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&(canvas_width as u32).to_le_bytes());
        file.extend_from_slice(&(height as u32).to_le_bytes());
        file.extend_from_slice(&[1, 0, 24, 0]);
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&((row_bytes * height as usize) as u32).to_le_bytes());
        // 到这里头是 14 + 40：剩下的 16 字节是两个像素密度与两张颜色表的计数
        file.extend_from_slice(&[0u8; 16]);
        for y in (0..height).rev() {
            for x in 0..canvas_width {
                let source = ((y * canvas_width + x) * 3) as usize;
                file.extend_from_slice(&rgb[source..source + 3]);
            }
            file.resize(file.len() + row_bytes - canvas_width as usize * 3, 0);
        }

        let path = std::env::temp_dir().join("litesound-marks.bmp");
        let mut handle = std::fs::File::create(&path).expect("BMP 写不了");
        handle.write_all(&file).expect("BMP 写一半");
        println!("标记图: {} ({width}x{height} 每格)", path.display());
    }
}
