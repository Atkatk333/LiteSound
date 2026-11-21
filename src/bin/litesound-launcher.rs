//! 开机自启专用的隐形启动器。
//!
//! 主程序是控制台子系统（CLI 的等待与退出码语义靠它，见 `src/main.rs` 头注释）。由 Run 键
//! 直接拉起一个控制台子系统程序时，Windows 在进程进入 `main` 之前就把 conhost 黑窗建好了，
//! `FreeConsole` 只能事后拆——冷启动时杀软扫描会把这段窗口期拉长，于是肉眼看见一次黑框。
//!
//! 本程序是 GUI 子系统，用 `CREATE_NO_WINDOW` 拉起主程序：子进程根本不分配控制台，没有窗口
//! 可闪。CLI 仍直接调 `litesound.exe`，本程序只负责自启动这一条路径。

#![windows_subsystem = "windows"]

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Threading::{
    CreateProcessW, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

/// 与被启动程序同目录的主程序文件名。
const MAIN_EXE: &str = "litesound.exe";

fn main() {
    let exe = match main_exe_path() {
        Some(path) => path,
        None => return fail("定位不到同目录的 litesound.exe"),
    };
    if let Err(error) = spawn_hidden(&exe) {
        fail(&format!("启动 {} 失败：{error}", exe.display()));
    }
}

/// 本程序所在目录下的 `litesound.exe`。
fn main_exe_path() -> Option<PathBuf> {
    let mut buf = vec![0u16; 32 * 1024];
    let len = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    let mut path = PathBuf::from(OsString::from_wide(&buf[..len]));
    path.set_file_name(MAIN_EXE);
    Some(path)
}

/// 以无控制台的方式启动，不等它结束。
fn spawn_hidden(exe: &Path) -> windows::core::Result<()> {
    let wide_path: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    // 命令行一律给带引号的完整路径：安装目录里有空格时，裸路径会被按空格劈成两段
    let mut command_line: Vec<u16> = format!("\"{}\"", exe.display())
        .encode_utf16()
        .chain([0])
        .collect();
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR(wide_path.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            None,
            &startup,
            &mut process,
        )?;
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

/// 自启动路径出问题时唯一能说话的地方：此时没有控制台，也不该碰主程序的 diag.txt。
fn fail(message: &str) {
    let message: Vec<u16> = message.encode_utf16().chain([0]).collect();
    unsafe {
        MessageBoxW(
            Some(HWND::default()),
            PCWSTR(message.as_ptr()),
            w!("LiteSound"),
            MB_OK | MB_ICONERROR,
        );
    }
}
