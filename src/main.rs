//! LiteSound 入口。
//!
//! 子系统选型是**控制台子系统**，而不是 `#![windows_subsystem = "windows"]`：
//! 托盘模式"不占控制台"是靠 `FreeConsole` 在运行期做到的（见 `tray::run`），而 GUI 子系统
//! 会连带毁掉 CLI 的契约——shell 对 GUI 子系统的进程**不等待、也不回填退出码**，于是
//! `--list --set x` 这类"靠退出码判定错误写法被拒"的检查全部失效（实测 pwsh 7.6 下
//! `$LASTEXITCODE` 为空）。CLI 与托盘共用一个可执行文件是规格定的，两边都得成立。

use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::Ime::ImmDisableIME;

mod audio;
mod banner;
mod cli;

// M1 交付时纯逻辑模块尚无调用方，M2 起逐个接入。M6（托盘）接入了 clicks 与 history，
// M4 的 state_file 也在这里被真正调起来，M7 的横幅（banner）把 fade 与 geometry 一并接上。
mod clicks;
mod diag;
mod dwtext;
mod fade;
mod geometry;
mod history;
mod menu;
mod model;
mod naming;
mod state_file;
mod tray;

fn main() {
    // 必须早于任何窗口创建、任何 DPI 相关度量的读取。
    declare_dpi_awareness();
    tray::enable_dark_menu_mode();
    disable_ime();
    diag::install_panic_hook();
    // 入口整体兜住（规格 5.4）：GUI 子系统进程 panic 时默认只弹一个裸错误框、每次登录弹
    // 一次，而日志里什么都不留。异常落盘后以退出码 3 结束。
    let code = match std::panic::catch_unwind(run) {
        Ok(code) => code,
        Err(_) => {
            diag::log("主流程异常中止，退出码 3");
            3
        }
    };
    std::process::exit(code);
}

/// 声明本进程的 DPI 感知级别：Per-Monitor V2。
///
/// 不声明的话本进程按 96 DPI 渲染，DWM 再把整幅位图拉伸到真实缩放——实测 150% 缩放下
/// （`HKCU\Control Panel\Desktop\WindowMetrics\AppliedDPI` = 144）右键菜单文字明显发虚发胖，
/// 托盘图标也因为 `SM_CXSMICON` 被虚拟化成 16 而选了 16 px 的帧去填 24 px 的槽位。
/// 声明之后菜单在真实 DPI 下原生渲染，`SM_CXSMICON` 也回到真实的 24（规格 4.2 里那些
/// 乘 `dpi_scale` 的横幅尺寸，同样以"进程知道自己处在什么 DPI 下"为前提）。
///
/// 失败一律忽略：清单里已经声明过、或系统不支持时它会返回错误，两者都不是需要处理的
/// 情况——本程序不读返回值。
fn declare_dpi_awareness() {
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// 关掉本进程所有线程的输入法（`ImmDisableIME(-1)`，`u32::MAX` 就是 `-1`）。
///
/// **必须早于任何窗口创建**：窗口一旦可见或被设为前台，TSF 就把当前输入法的 TIP DLL 装进本
/// 进程，TIP 还会自建十几个工作线程。本机实测（2026-09-27，同一探针 3 轮零离散）：只建隐藏
/// 窗口 1.97 MB / 31 模块；把窗口设为前台后 8.3 MB / 49 模块 / 18 线程，多出来的是微信输入法
/// 的 `wetype_tip.dll` + `wetype_tip_core.dll` + `CrashRpt1500.dll`（映像共 5.73 MB）。交付的
/// exe 上同一条台阶是"首次右键开菜单 2.31 MB → 9.20 MB、模块 39 → 59、线程 5 → 17"。
///
/// 触发点在 `tray::show_menu` 的 `SetForegroundWindow`，也就是每一次右键；历史上时中时不中，
/// 是因为前台锁——没有近期真实用户输入时那次调用会被系统拒绝（实测同一 harness 下基线
/// 第 1 轮注入、第 2 轮干净）。本程序没有任何文本输入面（托盘窗口从不显示、菜单是系统菜单、
/// 横幅自己画字），关掉输入法零功能损失，`SetForegroundWindow` 因此可以保留，坑 #10 要的
/// "点别处菜单会关"不必让路给内存。
///
/// 放在 `main` 而不是托盘路径里：`--bannertest` 也建可见窗口，它只活 5 秒、实测 3 轮都没被
/// 注入，但注入最早在第 6 秒出现，那是时序擦边而非机制免疫。CLI 各模式不建窗口，这一行对
/// 它们零影响。
///
/// 返回值不读：失败只可能是"本进程没有输入法可关"，那正是要的结果。
fn disable_ime() {
    let _ = unsafe { ImmDisableIME(u32::MAX) };
}

/// 参数分派与执行主体。
///
/// 与 `main` 拆开是为了让 `catch_unwind` 有个干净的可展开边界：把分派直接写在 `main` 里，
/// 兜底就只能包住整个进程体，连 `process::exit` 都在里面。
fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 显式分派（规格 2.1）：每个已知选项一条臂，且**连参数个数一起判**——题目里
    // "多余参数一律拒绝"就是靠 `args.len()` 落在这两条兜底臂上实现的，`--list --set x`
    // 因此不会静默当成 `--list` 跑掉。三种失败分得开：未知参数、已知但个数不对、压根没给。
    // 托盘模式是无参数与 `--tray` 两种写法（规格 2）；无参数那支必须放在最后——
    // `args.first()` 为空时只剩它一条路。
    match (args.first().map(String::as_str), args.len()) {
        (Some("--list"), 1) => cli::run_list(),
        (Some("--props"), 1) => cli::run_props(),
        (Some("--menu"), 1) => cli::run_menu(),
        (Some("--set"), 2) => cli::run_set(&args[1]),
        (Some("--bannertest"), 2) => cli::run_bannertest(&args[1]),
        (Some("--tray"), 1) => tray::run(),
        (Some("--version"), 1) => cli::run_version(),
        (Some("--help"), 1) => cli::run_help(),
        (Some(option), _) if cli::OPTIONS.contains(&option) => cli::bad_arg_count(option),
        (Some(option), _) => cli::unknown_option(option),
        (None, _) => tray::run(),
    }
}
