//! 命令行子命令。
//!
//! 参数分派在 `main` 里用显式 `match` 做（规格 2.1：不允许"谁都不匹配就落到默认分支"）；
//! 本模块只负责把已经决定好的子命令跑完并给出退出码。

use crate::audio;
use crate::banner;
use crate::history::{self, SwitchOutcome};
use crate::naming::{self, state_name};

/// `--list`：全部渲染端点。
///
/// 列 = 是否默认(`*`) / 状态名 / 排序键来源 / ID / 菜单名。取 `ALL_STATES` 是刻意的
/// （规格 6.4）：诊断要看得到 disabled / not-present / unplugged，菜单才只看 active。
pub fn run_list() -> i32 {
    let default = audio::default_id();
    let mut entries = audio::enumerate(audio::ALL_STATES);
    naming::number(&mut entries);

    for e in &entries {
        let marker = if history::same_id(default.as_deref(), Some(e.id.as_str())) {
            "*"
        } else {
            " "
        };
        println!(
            "{marker} {:<11} {:<7} {}  {}",
            state_name(e.state),
            e.sort_kind.as_str(),
            e.id,
            e.name
        );
    }
    0
}

/// `--props`：打印每台端点的**原始**属性值，用于排查命名与分类问题。
///
/// 打原始形态而不是折算后的名字：值为 0 或越界的时间戳折算后会变成"缺失"，
/// 而它们正是"顺序看着不对"最常见的成因——那时就需要看出"键在、值是 0"这件事。
///
/// `form` / `jack` / `icon` 三列是分类判据的取证面：`form` 是托盘图标的判据，`jack` 是它的
/// 上游（驱动声明的 KS pin category），`icon` 是蓝牙标记的判据。逐台一比才知道"设备被分成
/// 音响还是耳机"是固件写死的还是系统推的，也才知道有没有新索引号要收进判据表。
pub fn run_props() -> i32 {
    let raw = audio::props(audio::ALL_STATES);
    println!("--props：全部渲染端点的原始属性值（FILETIME 为原始计数，未做合法性判断）");
    for r in &raw {
        println!("{}", r.id);
        println!("  state    {} (0x{:X})", state_name(r.state), r.state);
        println!("  store    {}", if r.store_ok { "ok" } else { "打不开" });
        println!("  category {}", show_string(r.category.as_deref()));
        println!("  desc     {}", show_string(r.desc.as_deref()));
        println!("  install  {}", show_filetime(r.install));
        println!("  arrive   {}", show_filetime(r.arrive));
        println!(
            "  form     {}",
            match r.form_factor {
                Some(v) => format!("{v} ({})", form_factor_name(v)),
                None => "none".to_string(),
            }
        );
        println!("  jack     {}", show_string(r.jack_subtype.as_deref()));
        println!("  icon     {}", show_string(r.icon_path.as_deref()));
    }
    println!("共 {} 台", raw.len());
    0
}

/// 形态因子的可读名，与规格 6.3 的取值表一致。
///
/// 只为把 `--props` 的输出变成能直接判读的东西：逐台对着 jack/icon 两列看，才能看出
/// "AEB 推导"和"驱动原始声明"之间有没有分叉。
fn form_factor_name(v: u32) -> &'static str {
    match v {
        0 => "RemoteNetworkDevice",
        1 => "Speakers",
        2 => "LineLevel",
        3 => "Headphones",
        4 => "Microphone",
        5 => "Headset",
        6 => "Handset",
        7 => "UnknownDigitalPassthrough",
        8 => "SPDIF",
        9 => "DigitalAudioDisplay(HDMI)",
        10 => "Unknown",
        _ => "越界值",
    }
}

/// `--menu`：打印右键菜单的内容，供人工与真机菜单逐行对照（前置五验收项 5）。
///
/// 只取 `ACTIVE`（规格 3.5：菜单只含 active 端点）。分隔线与"退出"一并打出来——菜单
/// 确实有这两项，不打印的话验收时就无从对照；设备名走 [`menu_text`] 的截断+转义，
/// 与真机菜单同一套处理。
///
/// 行首那一列**就是真机菜单勾选位里的东西**：`*` 是绿点、`b` 是蓝牙符号、`B` 是两者同格
/// （既当前又蓝牙）。不这样打的话，"哪几行该有标记"这件事只能在真机上靠眼睛数，而位图
/// 建失败时这里是空的——一眼就能看出是判据没命中还是标记没画出来。
pub fn run_menu() -> i32 {
    let default = audio::default_id();
    let mut entries = audio::enumerate(audio::ACTIVE);
    naming::number(&mut entries);

    for e in &entries {
        let mark = match (
            history::same_id(default.as_deref(), Some(e.id.as_str())),
            e.bluetooth,
        ) {
            (true, true) => "B",
            (true, false) => "*",
            (false, true) => "b",
            (false, false) => " ",
        };
        println!("{mark} {}", naming::menu_text(&e.name));
    }
    println!("---");
    println!("  退出");
    0
}

/// `--set <ID>`：按 ID 精确匹配（忽略大小写）后切换并回读校验。
///
/// **匹配范围是 `ACTIVE`**：名字里带组内编号，而编号是"在当前切片里"算的（规格 6.6），
/// 用别的切片会打印出与菜单/tooltip 不一致的名字。这也意味着本命令与托盘菜单能切的
/// 是同一批端点，非 active 的 ID 一律按"找不到端点 ID"退出码 2 处理（规格 2.1）。
///
/// 退出码：成功与"已经是当前"都是 0（规格 2.1），失败 1，找不到 ID 2。
pub fn run_set(id: &str) -> i32 {
    let mut entries = audio::enumerate(audio::ACTIVE);
    naming::number(&mut entries);

    let Some(target) = entries.iter().find(|e| e.id.eq_ignore_ascii_case(id)) else {
        eprintln!("找不到端点 ID: {id}");
        return 2;
    };

    match audio::set_default(&target.id) {
        SwitchOutcome::Switched => {
            println!("已切换: {}", target.name);
            0
        }
        SwitchOutcome::AlreadyCurrent => {
            println!("已经是: {}", target.name);
            0
        }
        SwitchOutcome::Failed => {
            eprintln!("切换失败: {}", target.name);
            1
        }
    }
}

/// `--bannertest <文本>`：只画一次横幅，5 秒后自动退出。
///
/// 转给 `banner` 是因为消息泵与窗口都归它管：横幅的停留与淡出定时器要靠那个泵才跑得起来。
pub fn run_bannertest(text: &str) -> i32 {
    banner::run_bannertest(text)
}

/// `--version`：只打印版本号，可被脚本解析。
///
/// 取 `CARGO_PKG_VERSION` 而不是写字面量：版本只有 Cargo.toml 一处来源，改版本号时
/// 不会漏掉这里。
pub fn run_version() -> i32 {
    println!("{}", env!("CARGO_PKG_VERSION"));
    0
}

/// `--help`：用法写 **stdout**，退出码 0。
pub fn run_help() -> i32 {
    print!("{USAGE}");
    0
}

/// 参数不认识时的兜底：用法写 **stderr**，退出码 2（规格 2.1）。
pub fn usage_error() -> i32 {
    eprint!("{USAGE}");
    2
}

/// 未知参数：先点名是哪个，再给用法；都写 stderr，退出码 2。
///
/// 只说"用法"是不够的：把 `--tset` 打成 `--set` 这类错误，光看用法那一屏反而更难发现
/// 错在哪——而它又必须报错，不能落到默认分支上当成功跑掉（规格 2.1）。
pub fn unknown_option(option: &str) -> i32 {
    eprintln!("未知参数: {option}");
    usage_error()
}

/// 已知选项但参数个数不对（少给 ID、或多给了参数）：同样写 stderr，退出码 2。
///
/// 规格 2.1 点名的例子就是这一条：`--list --set x` 必须报错，静默忽略会让错误写法看
/// 起来成功了。
pub fn bad_arg_count(option: &str) -> i32 {
    eprintln!("参数个数不对: {option}");
    usage_error()
}

/// 已知选项一览。分派用它区分"未知参数"与"个数不对"两种报错，两处必须一起改。
pub const OPTIONS: [&str; 8] = [
    "--list",
    "--props",
    "--menu",
    "--set",
    "--bannertest",
    "--tray",
    "--version",
    "--help",
];

/// 用法文本。`--help` 与参数错误共用一份，免得两处说法不一致。
const USAGE: &str = "\
用法: litesound <子命令>

  --list         列出全部渲染端点（含 disabled / not-present / unplugged）
  --props        打印每台端点的原始属性值（含形态因子与图标索引），排查命名/分类问题用
  --menu         打印右键菜单的内容（仅 active 端点；行首 * = 绿点、b = 蓝牙标记）
  --set <ID>     按 ID 切换默认输出设备并回读校验
  --bannertest <文本>  只画一次横幅，5 秒后自动退出
  --tray         常驻托盘（等价于不带参数；已有实例在跑时静默退出）
  --version      只打印版本号
  --help         打印本用法
";

fn show_string(value: Option<&str>) -> String {
    match value {
        None => "(缺失)".to_string(),
        Some("") => "(空)".to_string(),
        Some(s) => s.to_string(),
    }
}

fn show_filetime(value: Option<i64>) -> String {
    match value {
        None => "(缺失)".to_string(),
        Some(v) => v.to_string(),
    }
}
