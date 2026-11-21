//! 状态文件（"上一台"记忆）：纯逻辑 + I/O。
//!
//! 【纯】部分（[`state_value`] / [`sweepable`]）不引用任何 Windows API，也不碰文件系统，
//! 输入全部以参数传入。I/O 部分（[`load`] / [`save`] / [`sweep`]）只用 `std::fs`，同样
//! 不碰 Windows API——判据留在纯函数里，I/O 只负责把它们喂饱。
//!
//! 这一层之所以要单独拎出来测，是因为它的两个坑（"清空"被旧值顶回来、清扫扫掉另一个
//! 进程刚写的临时文件）都是**逻辑错误**而不是 I/O 错误，不写测试就只能在真机上偶发。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

/// 临时文件的最小年龄（秒）。刚写的临时文件属于另一个还在跑的进程，不能清（坑 #20）。
pub const SWEEP_AGE_SECS: i64 = 60;

/// 状态文件名。
pub const STATE_NAME: &str = "state.txt";

/// 临时文件名前缀 / 后缀。
const TMP_PREFIX: &str = "state.txt.";
const TMP_SUFFIX: &str = ".tmp";
/// 旧版的固定名。托盘与 CLI 是两个进程，固定名会互相顶掉，所以现在带随机标签，
/// 但历史文件可能还在盘上，同样要能清掉。
const LEGACY_TMP_NAME: &str = "state.txt.tmp";

/// "确实没有上一台"的显式哨兵。
///
/// 写哨兵而不是写空串：空串要靠"读取端跳过空文件"才对得上，而那条约定一旦在任一处
/// 实现里走了样，清掉的记忆就会被另一处的旧值在 mtime 竞赛里顶回来（坑 #14）。
const NONE_SENTINEL: &str = "none";

/// 两处目录的**顺序**（规格 5.1）：先 exe 所在目录（绿色程序的本分），再
/// `%LOCALAPPDATA%\LiteSound`。
///
/// 顺序只有这一份，Step 19 的 diag.txt 选址会来调它——两处各写一份，迟早会分叉，
/// 然后长出"日志说写进了 A、状态其实读的是 B"这种只能靠猜的问题。
///
/// 返回**候选列表**而不是"第一个可用的"：读要两份都比 mtime（只按顺序取第一份，会在
/// exe 目录转只读之后永远读出旧值，新历史静默回退），写要两份都写。
pub fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(2);
    if let Some(dir) = exe_dir() {
        dirs.push(dir);
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        if !local.is_empty() {
            dirs.push(PathBuf::from(local).join("LiteSound"));
        }
    }
    dirs
}

fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.parent().map(Path::to_path_buf)
}

/// 读"上一台"。
///
/// 两处里**修改时间最新**的那份说了算（规格 5.2）：最新那份若是 `none` 哨兵，
/// 结果就是"没有上一台"——这与"两份都没有"同样返回 `None`，但含义不同，调用方不需要
/// 区分（左键退回菜单是同一个动作）。
pub fn load() -> Option<String> {
    data_dirs()
        .iter()
        .filter_map(|dir| read_state(dir))
        .max_by_key(|(mtime, _)| *mtime)
        .and_then(|(_, raw)| state_value(&raw))
}

/// 读一处；返回 `(修改时间, 原文)`。
///
/// 文件不在、元数据拿不到、内容不是文本，一律静默跳过：另一处仍有机会，而这里能做的
/// 事只有"当作这份不存在"——回退成空串反而会把另一处的有效记忆顶掉。
fn read_state(dir: &Path) -> Option<(SystemTime, String)> {
    let path = dir.join(STATE_NAME);
    let mtime = fs::metadata(&path).ok()?.modified().ok()?;
    let raw = fs::read_to_string(&path).ok()?;
    Some((mtime, raw))
}

/// 写"上一台"：**两处都写**（规格 5.2）。
///
/// 只写第一个能写的，会让两份从此分道扬镳：之后任一处变得不可读，读到的是另一处的旧值，
/// 而它们再也不会自己合回去。
///
/// 失败只在一处都不成时记日志。单处失败（典型是装在 Program Files 下、exe 目录只读）
/// 是常态且无害——load 取最新 mtime，另一处照样生效；每次切换都记一条就成了噪声，
/// 而 diag.txt 的价值恰恰在于"有内容就是有事"。真要排查时，两处的原因会一起写进那一行。
pub fn save(id: Option<&str>) {
    let text = id.unwrap_or(NONE_SENTINEL);
    let dirs = data_dirs();
    let mut failures: Vec<String> = Vec::new();

    for dir in &dirs {
        match write_atomic(dir, text) {
            Ok(()) => sweep(dir),
            Err(e) => failures.push(format!("{}: {e}", dir.display())),
        }
    }

    if !dirs.is_empty() && failures.len() == dirs.len() {
        crate::diag::log(&format!(
            "状态文件两处都写不进（{}），本次切换不会被记住",
            failures.join("；")
        ));
    }
}

/// 原子写一处：先写带随机标签的临时文件，再改名覆盖 `state.txt`（规格 5.2）。
///
/// 改名走 `fs::rename`，它在 Windows 上是 `MoveFileEx(MOVEFILE_REPLACE_EXISTING)`，
/// 可以直接覆盖已存在的目标，不需要先删（先删会开一个"目标不存在"的窗口）。
fn write_atomic(dir: &Path, text: &str) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let tmp = dir.join(temp_name());
    fs::write(&tmp, text)?;
    match fs::rename(&tmp, dir.join(STATE_NAME)) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 改名失败就自己收掉临时文件，别留到下一次 sweep
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 临时文件名：`state.txt.<标签>.tmp`——必须落在 [`sweepable`] 认的那一类里，
/// 否则它永远清不掉。
///
/// 标签 = 进程号 + 纳秒时间 + 本进程内的序号：托盘与 CLI 是两个进程，标签的作用正是
/// 让它们同时写时不会互相覆盖/删掉对方正在写的那个文件。不引随机数依赖，这三样凑起来
/// 已经足够（进程号保证跨进程不撞，序号保证同进程同一纳秒内不撞）。
fn temp_name() -> String {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "{TMP_PREFIX}{:x}-{nanos:x}-{seq:x}{TMP_SUFFIX}",
        std::process::id()
    )
}

/// 清扫一处的过期临时文件。
///
/// 尽力而为：目录读不到、某个文件删不掉都不算错——它们只是限时的垃圾，下一次写还会再来
/// 一遍。两条判据（名字、年龄）都在纯函数 [`sweepable`] 里，这里只负责取事实。
fn sweep(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();

    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // 取不到修改时间（或时间在未来）按年龄 0 算：宁可不清，也不能清掉别人正在写的
        let age = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| now.duration_since(m).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if sweepable(&name, age) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// 解析状态文件内容。
///
/// 返回 `None` 表示"确实没有上一台"——**空文件与 `none` 是同一件事**，都必须能写进文件。
/// 若改成"写空串表示清空"而读取端又跳过空文件，清掉的记忆会被另一处的旧值在 mtime
/// 竞赛中顶回来，下次启动无声复活（坑 #14）。
pub fn state_value(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return None;
    }
    Some(trimmed.to_string())
}

/// 一个临时文件是否该被清扫。
///
/// 两条**同时成立**：名字是临时文件，且修改时间距今超过 60 秒。
/// `state.txt` 本身永不清理——它是真正的记忆，不是中间产物。
pub fn sweepable(name: &str, mtime_age_secs: i64) -> bool {
    let is_tmp = name == LEGACY_TMP_NAME
        || (name.starts_with(TMP_PREFIX)
            && name.ends_with(TMP_SUFFIX)
            && name.len() > TMP_PREFIX.len() + TMP_SUFFIX.len());

    is_tmp && mtime_age_secs > SWEEP_AGE_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 9.3 状态值解析 ----

    #[test]
    fn empty_content_means_cleared() {
        assert_eq!(state_value(""), None);
    }

    #[test]
    fn whitespace_only_means_cleared() {
        assert_eq!(state_value("   \t\r\n  "), None);
    }

    #[test]
    fn none_sentinel_means_cleared_case_insensitively() {
        assert_eq!(state_value("none"), None);
        assert_eq!(state_value("NONE"), None);
        assert_eq!(state_value("NoNe"), None);
        assert_eq!(state_value("  none\n"), None);
    }

    #[test]
    fn id_with_newlines_is_trimmed() {
        assert_eq!(
            state_value("{0.0.0.00000000}.{abc}\r\n"),
            Some("{0.0.0.00000000}.{abc}".to_string())
        );
        assert_eq!(state_value("\n{id}\n"), Some("{id}".to_string()));
    }

    #[test]
    fn normal_id_is_kept_as_is() {
        assert_eq!(state_value("{id}"), Some("{id}".to_string()));
        assert_eq!(state_value("  {id}  "), Some("{id}".to_string()));
    }

    #[test]
    fn id_merely_containing_none_is_not_cleared() {
        assert_eq!(state_value("none-1"), Some("none-1".to_string()));
        assert_eq!(state_value("{none}"), Some("{none}".to_string()));
    }

    // ---- 9.3 临时文件清扫 ----

    #[test]
    fn legacy_fixed_name_is_swept_when_old() {
        assert!(sweepable("state.txt.tmp", 3600));
        assert!(sweepable("state.txt.tmp", SWEEP_AGE_SECS + 1));
    }

    #[test]
    fn legacy_fixed_name_is_kept_when_fresh() {
        assert!(!sweepable("state.txt.tmp", SWEEP_AGE_SECS));
        assert!(!sweepable("state.txt.tmp", 0));
        assert!(!sweepable("state.txt.tmp", -5));
    }

    #[test]
    fn labeled_temp_name_is_swept_when_old() {
        assert!(sweepable("state.txt.1234.tmp", 3600));
        assert!(sweepable("state.txt.abcd-ef.tmp", SWEEP_AGE_SECS + 1));
    }

    #[test]
    fn labeled_temp_name_is_kept_when_fresh() {
        assert!(!sweepable("state.txt.1234.tmp", SWEEP_AGE_SECS));
        assert!(!sweepable("state.txt.1234.tmp", 0));
    }

    #[test]
    fn state_file_itself_is_never_swept() {
        assert!(!sweepable("state.txt", 3600));
        assert!(!sweepable("state.txt", i64::MAX));
    }

    #[test]
    fn unrelated_files_are_never_swept() {
        assert!(!sweepable("diag.txt", 3600));
        assert!(!sweepable("state.txt.bak", 3600));
        assert!(!sweepable("state.txt.tmp.bak", 3600));
        assert!(!sweepable("other.txt.1234.tmp", 3600));
        assert!(!sweepable("state.tmp", 3600));
    }

    // ---- 5.2 I/O 往返 ----
    //
    // 这几个测试不经过 data_dirs()——那两处指向真实的 exe 目录与 %LOCALAPPDATA%，在测试里
    // 会污染构建产物和用户目录。直接打按目录取的私有函数，要证的是三件事：判据确实被用上了、
    // Windows 上"改名覆盖已存在的 state.txt"确实成立、以及生成的临时文件名确实落在可清扫类里。

    /// 测试用临时目录，带 RAII 清理。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("litesound-test-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("建临时目录");
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        /// 目录下所有文件名，排序后返回，便于断言"留下了什么"。
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .expect("读临时目录")
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn write_then_read_round_trips_and_overwrites() {
        let dir = TempDir::new("roundtrip");

        write_atomic(&dir.0, "{a}").expect("首次写");
        assert_eq!(read_state(&dir.0).expect("读回").1, "{a}");

        // 第二次写必须覆盖成功：Windows 的 rename 覆盖语义是原子写的前提，
        // 若它退化成"目标存在就失败"，这里会红，而真机上只会表现为"记忆永远停在第一次"
        write_atomic(&dir.0, "{b}").expect("覆盖写");
        assert_eq!(read_state(&dir.0).expect("读回").1, "{b}");

        assert_eq!(
            dir.names(),
            vec![STATE_NAME.to_string()],
            "写完不留临时文件"
        );
    }

    #[test]
    fn cleared_state_is_written_as_sentinel_and_reads_back_as_none() {
        let dir = TempDir::new("sentinel");

        write_atomic(&dir.0, NONE_SENTINEL).expect("写哨兵");
        let raw = read_state(&dir.0).expect("读回").1;
        assert_eq!(raw, NONE_SENTINEL);
        assert_eq!(state_value(&raw), None, "哨兵必须解析成「没有上一台」");
    }

    #[test]
    fn generated_temp_name_is_in_the_sweepable_class() {
        // 名字自己算，不复制一份"看起来像"的字符串：否则改了 temp_name 的格式，
        // 这条断言照样绿，而真机上临时文件再也清不掉
        let name = temp_name();
        assert!(sweepable(&name, SWEEP_AGE_SECS + 1), "{name}");
        // 新写的临时文件属于另一个还在跑的进程，绝不能被清
        assert!(!sweepable(&name, SWEEP_AGE_SECS), "{name}");
    }

    #[test]
    fn sweep_removes_only_old_temp_files() {
        let dir = TempDir::new("sweep");
        let old = dir.path("state.txt.aaaa.tmp");
        let fresh = dir.path("state.txt.bbbb.tmp");

        write_atomic(&dir.0, "{id}").expect("写状态");
        fs::write(&old, "x").expect("造旧临时文件");
        fs::write(&fresh, "x").expect("造新临时文件");

        // 把旧文件的修改时间推到 1 小时前
        let past = SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&old)
            .expect("打开旧临时文件")
            .set_modified(past)
            .expect("改修改时间");

        sweep(&dir.0);

        assert_eq!(
            dir.names(),
            vec![STATE_NAME.to_string(), "state.txt.bbbb.tmp".to_string()],
            "只清旧的临时文件，state.txt 与还在写的临时文件都必须留下"
        );
    }
}
