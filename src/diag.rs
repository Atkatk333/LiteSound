//! 诊断日志（diag.txt）。
//!
//! 只在**错误/异常**时被调用：正常运行一个字都不写，所以"文件有内容"本身就是信号。
//!
//! 目录顺序与 state.txt 共用 [`crate::state_file::data_dirs`]（规格 5.1）——两处各写
//! 一份顺序迟早分叉，然后长出"日志说写进了 A、状态其实读的是 B"这种只能靠猜的问题。
//! 但与 state.txt 不同，diag.txt 只写**第一个证明写得动的**那一处（5.3）：它是单向的
//! 只追加记录，没有"两份取最新"的必要，重复写两份只会让排查时不知道该看哪份。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use windows::Win32::System::SystemInformation::GetLocalTime;

/// 日志文件名。
pub const DIAG_NAME: &str = "diag.txt";

/// 已**证明写得动**的目录。
///
/// `None` 有两种含义，都是"下次还要重新试"：进程启动至今还没写过，以及上一次候选目录
/// 全部试失败。失败不缓存是规格 5.3 点名的：目录后来变得可写（U 盘插回来、权限改回来）
/// 之后，缓存的失败会让日志再也写不进去。
static CHOSEN: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn chosen() -> &'static Mutex<Option<PathBuf>> {
    CHOSEN.get_or_init(|| Mutex::new(None))
}

/// 记一条诊断信息。格式 `HH:mm:ss.fff` + 两空格 + 消息 + 换行。
///
/// **本函数不返回错误、不抛异常**（规格 5.3）：记不下来一件事，不该把调用方一起带下水——
/// 所有调用点都在错误路径上，那里再炸一次就什么都留不下了。
pub fn log(msg: &str) {
    let line = line(msg);
    let mut slot = chosen().lock().unwrap_or_else(|e| e.into_inner());

    // 先试缓存的那处。它也可能失败（盘被拔、目录被删、文件被独占），失败就当没缓存过，
    // 走下面重新选址——不然一次瞬时失败会让这个进程余生都写不出日志。
    if let Some(dir) = slot.as_ref() {
        if append(dir, &line) {
            return;
        }
        *slot = None;
    }

    for dir in crate::state_file::data_dirs() {
        if append(&dir, &line) {
            *slot = Some(dir);
            return;
        }
    }
    // 两处都写不进：静默返回
}

/// 一行日志的完整形态：`HH:mm:ss.fff` + 两空格 + 消息 + 换行。
///
/// 提成函数只为让格式本身可断言——它是这份日志唯一的对外契约，直接埋在 `log` 里就只能靠
/// 人眼看真机文件。
fn line(msg: &str) -> String {
    format!("{}  {msg}\n", timestamp())
}

/// 装 panic 钩子：把 panic 的位置与消息写进 diag.txt（规格 5.4）。
///
/// 链到原有钩子而不是替换掉：GUI 子系统下原钩子的输出本来也无处可去（无害），CLI 模式下
/// 会多一条 stderr 输出、便于当场排查；而单元测试里若不链，任何断言失败的信息都会被本钩子
/// 吞掉，测试立刻变得没法调试。
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&describe_panic(info));
        previous(info);
    }));
}

/// panic 的一行描述：`panic @ 文件:行:列: 消息`。
fn describe_panic(info: &std::panic::PanicHookInfo<'_>) -> String {
    let place = match info.location() {
        Some(l) => format!("{}:{}:{}", l.file(), l.line(), l.column()),
        None => "位置未知".to_string(),
    };
    let payload = info.payload();
    let message = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        // panic_any 可以扔任意负载，这条分支保证不因拿不到消息而丢掉整条记录
        "（非字符串负载）".to_string()
    };
    format!("panic @ {place}: {message}")
}

/// 把一段代码整体兜住，供 FFI 边界（窗口过程）使用（规格 5.4）。
///
/// panic 跨过 `extern "system"` 边界是未定义行为：默认展开会让进程当场崩掉，且日志里
/// 什么都留不下（GUI 子系统连控制台都没有）。所以两个窗口过程都必须用它包住整个函数体。
pub fn guard<R>(what: &str, body: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(value) => Some(value),
        Err(_) => {
            // 具体位置与消息已由钩子写过；这里补的是"哪一段没跑完"——只有钩子那一行的话，
            // 排查时看不出异常是从哪个回调里冒出来的
            log(&format!("{what} 内异常，该段处理已中止"));
            None
        }
    }
}

/// 真正写一次，返回值即"写得动吗"。
///
/// 选址必须**用真正的写入去试**（规格 5.3）：只问"目录能不能创建"是错的——一个存在但
/// 只读的目录（Program Files、U 盘、网络盘），或文件被别的进程占住，都会永远回答"能"，
/// 于是日志被静默丢掉，而排查的人以为"没事发生"。
fn append(dir: &Path, line: &str) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(DIAG_NAME))
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .is_ok()
}

/// `HH:mm:ss.fff`，**本地时间**。
///
/// 用 `GetLocalTime` 而不是把 `SystemTime` 直接格式化：后者是 UTC，日志里就会出现"我
/// 14:32 点的，日志写着 06:32"这种对不上的时间——诊断日志的价值全在能和人的动作对齐。
fn timestamp() -> String {
    let st = unsafe { GetLocalTime() };
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("litesound-diag-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn assert_is_hms_millis(ts: &str) {
        assert_eq!(ts.len(), 12, "「{ts}」不是 HH:mm:ss.fff");
        let bytes = ts.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            match i {
                2 | 5 => assert_eq!(*b, b':', "「{ts}」第 {i} 位应为冒号"),
                8 => assert_eq!(*b, b'.', "「{ts}」第 {i} 位应为点"),
                _ => assert!(b.is_ascii_digit(), "「{ts}」第 {i} 位不是数字"),
            }
        }
    }

    #[test]
    fn timestamp_has_the_documented_shape() {
        assert_is_hms_millis(&timestamp());
    }

    #[test]
    fn line_layout_is_timestamp_two_spaces_message_newline() {
        let l = line("m");
        assert_is_hms_millis(&l[..12]);
        assert_eq!(&l[12..14], "  ");
        assert_eq!(&l[14..15], "m");
        assert_eq!(&l[15..], "\n");
        assert_eq!(l.len(), 12 + 2 + 1 + 1);
    }

    #[test]
    fn append_writes_and_keeps_history() {
        let dir = temp_dir("append");
        assert!(append(&dir, "第一行\n"));
        assert!(append(&dir, "第二行\n"));

        // 只追加不覆盖：日志是随时间累积的，被覆盖掉的最后一行等于没有
        assert_eq!(
            fs::read_to_string(dir.join(DIAG_NAME)).expect("读回"),
            "第一行\n第二行\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_refuses_a_target_that_cannot_be_a_directory() {
        // 拿"已存在的文件"顶掉目录位。要证的是选址确实靠真写：若它误报成功，缓存就会
        // 锁死在一个根本写不进的位置，而日志此后静默消失（规格 5.3 点名的那个坑）
        let dir = temp_dir("blocked");
        let blocked = dir.join("blocked");
        fs::write(&blocked, "x").expect("造阻塞文件");

        assert!(!append(&blocked, "x\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn panic_hook_writes_location_and_message_to_a_candidate_dir() {
        // marker 带进程号：上一次测试运行留下的 diag.txt 不会让这条断言假绿
        let marker = format!("panic-hook-test-{:x}", std::process::id());
        let dirs = crate::state_file::data_dirs();
        let existed: Vec<bool> = dirs.iter().map(|d| d.join(DIAG_NAME).exists()).collect();

        install_panic_hook();
        let caught = std::panic::catch_unwind(|| panic!("{marker}"));
        assert!(caught.is_err(), "catch_unwind 必须接得住");

        // 真正走一趟 data_dirs 选址：这条断言的另一半价值就是证明两处候选目录里至少有一处
        // 在真机上写得动（全写不动时本程序的日志能力等于零，而这在测试里会当场暴露）
        let (dir, existed_before) = dirs
            .iter()
            .zip(existed.iter())
            .find_map(|(dir, existed_before)| {
                let text = fs::read_to_string(dir.join(DIAG_NAME)).ok()?;
                text.contains(&marker)
                    .then(|| (dir.clone(), *existed_before))
            })
            .expect("panic 必须落进某一处候选目录的 diag.txt");

        let text = fs::read_to_string(dir.join(DIAG_NAME)).expect("读回");
        let logged = text
            .lines()
            .find(|l| l.contains(&marker))
            .expect("含 marker 的那一行");

        assert_is_hms_millis(&logged[..12]);
        assert_eq!(&logged[12..14], "  ");
        assert!(logged.contains("diag.rs"), "必须带 panic 位置: {logged}");
        assert!(logged[14..].starts_with("panic @ "), "{logged}");

        // 只删本测试新建的：真机排查在看的 diag.txt 不能被测试顺手清掉
        if !existed_before {
            let _ = fs::remove_file(dir.join(DIAG_NAME));
        }
    }
}
