//! 设备名预处理、标签拼装、排序与组内编号。
//!
//! 【纯】：不引用任何 Windows API，输入全部以参数传入。

use crate::model::Entry;
use std::cmp::Ordering;
use std::collections::HashMap;

/// 剥掉 Windows 自带的 `N- ` 前缀。
///
/// 该前缀只反映**设备接口描述冲突**，与"同类别有几台"无关——实测 4 个"耳机"里
/// 只有 1 个被编号。若不剥，菜单里会出现 `耳机 (2- 蓝牙耳机A)` 这种噪声。
///
/// 规则：找到第一个 `"- "`，若其前全为数字且非空，则取其后部分；结果为空则原样返回。
pub fn strip_win_prefix(desc: &str) -> String {
    if let Some(pos) = desc.find("- ") {
        let head = &desc[..pos];
        let tail = &desc[pos + 2..];
        if !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()) && !tail.is_empty() {
            return tail.to_string();
        }
    }
    desc.to_string()
}

/// 拼装菜单名。
///
/// | 条件 | 结果 |
/// |---|---|
/// | 类别为空 | 只用描述 |
/// | 描述为空 | 只用类别（**不可拼成"耳机 ()"**） |
/// | 组内总数 < 2，或本机排第 1 | `类别 (描述)` |
/// | 否则 | `类别 N (描述)` |
pub fn label(category: &str, desc: &str, rank: u32, total: u32) -> String {
    if category.is_empty() {
        return desc.to_string();
    }
    if desc.is_empty() {
        return category.to_string();
    }
    if total < 2 || rank == 1 {
        format!("{category} ({desc})")
    } else {
        format!("{category} {rank} ({desc})")
    }
}

/// 排序比较器：接入时间 → 安装时间 → ID（序数、区分大小写）。
pub fn by_order(a: &Entry, b: &Entry) -> Ordering {
    a.order_key().cmp(&b.order_key())
}

/// 先按 `by_order` 排序，再按**同类别**分组编号。
///
/// 计数必须在**当前切片**范围内做，不能在"全部设备"范围内做：4 台耳机里只连了 2 台时
/// 结果应是 `耳机` / `耳机 2`，出现 `耳机` / `耳机 3` 这种空洞就是算错了批次。
pub fn assign_ranks(entries: &mut [Entry]) {
    entries.sort_by(by_order);

    let mut totals: HashMap<String, u32> = HashMap::new();
    for e in entries.iter() {
        *totals.entry(e.category.clone()).or_insert(0) += 1;
    }

    let mut seen: HashMap<String, u32> = HashMap::new();
    for e in entries.iter_mut() {
        let n = seen.entry(e.category.clone()).or_insert(0);
        *n += 1;
        e.rank = *n;
        e.total = totals[&e.category];
    }
}

/// 命名流水线一步到位：剥前缀 → 排序 → 组内编号 → 写回 `name`。
///
/// 顺序照规格 6.6 的三段（描述预处理 → 标签拼装 → 编号规则）；`desc` 在这里被就地剥掉
/// 前缀，`--props` 想看的原始属性值由它自己直接读属性库，不经过本函数。
pub fn number(entries: &mut [Entry]) {
    for e in entries.iter_mut() {
        e.desc = strip_win_prefix(&e.desc);
        let kind = e.sort_kind();
        e.sort_kind = kind;
    }

    assign_ranks(entries);

    for e in entries.iter_mut() {
        e.name = label(&e.category, &e.desc, e.rank, e.total);
    }
}

/// FILETIME 的合法上界：9999-12-31T23:59:59.9999999Z 的 FILETIME 计数。
///
/// 取 .NET `DateTime.MaxValue.ToFileTimeUtc()` 的实测值 2650467743999999999（反解回
/// 9999-12-31 23:59:59）。用它作参照是因为规格里"越界值会让时间转换直接抛异常"这句话
/// 所指的转换实现就是它——实测 `DateTime.MinValue.ToFileTimeUtc()` 抛
/// "Not a valid Win32 FileTime."，即越界与非法是同一类错误。
pub const MAX_FILETIME: u64 = 2_650_467_743_999_999_999;

/// 状态位 → 可读名字。
///
/// **`2` 是 disabled，`8` 才是 unplugged**，写反会让诊断输出骗人（坑 #2）。
/// 四个已知值之外的按十六进制打印，避免把未知状态静默显示成某一类。
pub fn state_name(state: u32) -> String {
    match state {
        1 => "active".to_string(),
        2 => "disabled".to_string(),
        4 => "not-present".to_string(),
        8 => "unplugged".to_string(),
        other => format!("0x{other:X}"),
    }
}

/// 把属性库读到的原始 FILETIME 折算成排序键，非法一律判"缺失"。
///
/// `<= 0` 与上界之外的都要挡掉：0 会变成 1601-01-01，一个"合法但荒谬"的排序键，
/// 会把某台设备无声顶到组内第一且不编号；越界的值则会让时间转换直接抛异常。
pub fn safe_filetime(raw: i64) -> Option<u64> {
    if raw <= 0 {
        return None;
    }
    let ticks = raw as u64;
    if ticks > MAX_FILETIME {
        return None;
    }
    Some(ticks)
}

/// 菜单文本里的 `&` 翻倍。
///
/// `&` 是菜单助记符，不翻倍会把 `B&O` 显示成 `BO`，还会吃掉一次 Alt。
/// 必须**先截断后转义**：反过来的话截断可能劈开已成对的 `&&`（坑 #18）。
pub fn menu_esc(text: &str) -> String {
    text.replace('&', "&&")
}

/// 设备名的显示上限（字符数）。菜单项与 tooltip 共用同一个值（规格 3.5 与 3.4）。
pub const MAX_CHARS: usize = 60;

/// 菜单项文本：**先截断到 60 字符，再把 `&` 翻倍**（规格 3.5 与坑 #18）。
///
/// 提成本模块的公开函数，而不是让 `cli` 与 `tray` 各留一份：真机菜单与 `--menu` 输出必须
/// 逐字一致（验收项 5 就是拿它俩对照的），两份实现迟早会在某次改动里分叉。
pub fn menu_text(name: &str) -> String {
    menu_esc(truncate_chars(name, MAX_CHARS))
}

/// 按**字符数**截断（不是字节数——60 个汉字在 150% 缩放下就是 1700+ px）。
///
/// 返回切片而非新字符串，方便调用方先截断再测量、再绘制，保证"量的就是要画的那串"（坑 #4）。
pub fn truncate_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        id: &str,
        category: &str,
        desc: &str,
        install: Option<u64>,
        arrive: Option<u64>,
    ) -> Entry {
        Entry {
            id: id.to_string(),
            state: 1,
            category: category.to_string(),
            desc: desc.to_string(),
            install,
            arrive,
            rank: 0,
            total: 0,
            name: String::new(),
            sort_kind: crate::model::SortKind::Install,
            bluetooth: false,
        }
    }

    // ---- 9.1 剥前缀 ----

    #[test]
    fn strips_numeric_prefix() {
        assert_eq!(strip_win_prefix("2- X"), "X");
        assert_eq!(strip_win_prefix("12- X"), "X");
    }

    #[test]
    fn keeps_desc_without_prefix() {
        assert_eq!(strip_win_prefix("蓝牙耳机A"), "蓝牙耳机A");
    }

    #[test]
    fn keeps_non_numeric_prefix() {
        assert_eq!(strip_win_prefix("a- x"), "a- x");
    }

    #[test]
    fn keeps_prefix_when_remainder_empty() {
        assert_eq!(strip_win_prefix("2- "), "2- ");
        assert_eq!(strip_win_prefix("2-"), "2-");
    }

    // ---- 9.1 标签拼装 ----

    #[test]
    fn label_numbers_second_of_four() {
        assert_eq!(label("耳机", "蓝牙耳机A", 2, 4), "耳机 2 (蓝牙耳机A)");
    }

    #[test]
    fn label_omits_number_for_first_in_group() {
        assert_eq!(label("耳机", "蓝牙耳机A", 1, 4), "耳机 (蓝牙耳机A)");
    }

    #[test]
    fn label_omits_number_for_single_member_group() {
        assert_eq!(label("扬声器", "内置声卡", 1, 1), "扬声器 (内置声卡)");
    }

    #[test]
    fn label_uses_desc_only_when_category_empty() {
        assert_eq!(label("", "蓝牙耳机A", 2, 4), "蓝牙耳机A");
    }

    #[test]
    fn label_uses_category_only_when_desc_empty() {
        assert_eq!(label("耳机", "", 2, 4), "耳机");
        assert_ne!(label("耳机", "", 2, 4), "耳机 ()");
    }

    // ---- 9.1 编号按批次算，不跳号 ----

    fn earphone(id: &str, desc: &str, install: u64) -> Entry {
        entry(id, "耳机", desc, Some(install), None)
    }

    #[test]
    fn number_assigns_ranks_in_install_order() {
        let mut all = vec![
            earphone("{1}", "内置声卡", 100),
            earphone("{2}", "蓝牙耳机A", 200),
            earphone("{3}", "蓝牙耳机B", 300),
            earphone("{4}", "USB耳机", 400),
            entry("{5}", "扬声器", "显示器音频", Some(100), None),
        ];
        number(&mut all);

        let names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"耳机 (内置声卡)"));
        assert!(names.contains(&"耳机 2 (蓝牙耳机A)"));
        assert!(names.contains(&"耳机 3 (蓝牙耳机B)"));
        assert!(names.contains(&"耳机 4 (USB耳机)"));
        assert!(names.contains(&"扬声器 (显示器音频)"));
    }

    #[test]
    fn ranks_and_totals_are_one_based_within_each_category() {
        // 9.1 原话："4 台 → 1/4、2/4、3/4、4/4；另一类 1 台 → 1/1"。
        // 名字只能间接反映 rank（rank=1 时名字里没有编号），所以 rank/total 必须单独断言。
        let mut all = vec![
            earphone("{1}", "内置声卡", 100),
            earphone("{2}", "蓝牙耳机A", 200),
            earphone("{3}", "蓝牙耳机B", 300),
            earphone("{4}", "USB耳机", 400),
            entry("{5}", "扬声器", "显示器音频", Some(100), None),
        ];
        number(&mut all);

        let ears: Vec<&Entry> = all.iter().filter(|e| e.category == "耳机").collect();
        assert_eq!(ears.len(), 4);
        for (i, e) in ears.iter().enumerate() {
            assert_eq!(e.rank, i as u32 + 1, "第 {} 台耳机的 rank 不对", i + 1);
            assert_eq!(e.total, 4);
        }

        let speaker = all.iter().find(|e| e.category == "扬声器").unwrap();
        assert_eq!(speaker.rank, 1);
        assert_eq!(speaker.total, 1);
    }

    #[test]
    fn number_does_not_skip_when_only_two_are_connected() {
        // 4 台耳机里只有第 1、3 台在当前批次 → 必须是 耳机 / 耳机 2，不能是 耳机 / 耳机 3
        let mut batch = vec![
            earphone("{1}", "内置声卡", 100),
            earphone("{3}", "蓝牙耳机B", 300),
        ];
        number(&mut batch);

        assert_eq!(batch[0].name, "耳机 (内置声卡)");
        assert_eq!(batch[0].rank, 1);
        assert_eq!(batch[0].total, 2);
        assert_eq!(batch[1].name, "耳机 2 (蓝牙耳机B)");
        assert_eq!(batch[1].rank, 2);
        assert_eq!(batch[1].total, 2);
    }

    // ---- 9.1 排序键 ----

    #[test]
    fn arrive_wins_over_install_and_reorders_numbering() {
        // 后装但先连的排前面，且编号跟着走一遍（只比排序先后测不到编号）
        let mut batch = vec![
            entry("{late}", "耳机", "后装先连", Some(900), Some(100)),
            entry("{early}", "耳机", "先装后连", Some(100), Some(900)),
        ];
        number(&mut batch);

        assert_eq!(batch[0].id, "{late}");
        assert_eq!(batch[0].name, "耳机 (后装先连)");
        assert_eq!(batch[1].id, "{early}");
        assert_eq!(batch[1].name, "耳机 2 (先装后连)");
    }

    #[test]
    fn missing_arrive_falls_back_to_install() {
        let mut batch = vec![
            entry("{b}", "耳机", "只有安装时间B", Some(900), None),
            entry("{a}", "耳机", "只有安装时间A", Some(100), None),
        ];
        number(&mut batch);

        assert_eq!(batch[0].id, "{a}");
        assert_eq!(batch[1].id, "{b}");
    }

    #[test]
    fn sort_kind_is_marked_on_entries() {
        let mut batch = vec![
            entry("{a}", "耳机", "有接入时间", Some(100), Some(500)),
            entry("{b}", "耳机", "没接入时间", Some(200), None),
        ];
        number(&mut batch);

        let a = batch.iter().find(|e| e.id == "{a}").unwrap();
        let b = batch.iter().find(|e| e.id == "{b}").unwrap();
        assert_eq!(a.sort_kind, crate::model::SortKind::Arrive);
        assert_eq!(a.sort_kind.as_str(), "arrive");
        assert_eq!(b.sort_kind, crate::model::SortKind::Install);
        assert_eq!(b.sort_kind.as_str(), "install");
    }

    #[test]
    fn number_strips_prefix_before_labeling() {
        let mut batch = vec![
            earphone("{1}", "1- 蓝牙耳机A", 100),
            earphone("{2}", "2- 蓝牙耳机B", 200),
        ];
        number(&mut batch);

        assert_eq!(batch[0].desc, "蓝牙耳机A");
        assert_eq!(batch[0].name, "耳机 (蓝牙耳机A)");
        assert_eq!(batch[1].name, "耳机 2 (蓝牙耳机B)");
    }

    // ---- 9.1 状态名 ----

    #[test]
    fn state_name_maps_the_four_known_bits() {
        assert_eq!(state_name(1), "active");
        assert_eq!(state_name(2), "disabled");
        assert_eq!(state_name(4), "not-present");
        assert_eq!(state_name(8), "unplugged");
    }

    #[test]
    fn state_name_does_not_swap_disabled_and_unplugged() {
        // 坑 #2：2 与 8 写反会让诊断输出骗人
        assert_ne!(state_name(2), "unplugged");
        assert_ne!(state_name(8), "disabled");
    }

    #[test]
    fn state_name_falls_back_to_hex() {
        assert_eq!(state_name(0), "0x0");
        assert_eq!(state_name(16), "0x10");
        assert_eq!(state_name(0xF), "0xF");
    }

    // ---- 9.1 FILETIME ----

    #[test]
    fn safe_filetime_rejects_zero_and_negative() {
        assert_eq!(safe_filetime(0), None);
        assert_eq!(safe_filetime(-1), None);
        assert_eq!(safe_filetime(i64::MIN), None);
    }

    #[test]
    fn safe_filetime_rejects_out_of_range() {
        assert_eq!(safe_filetime(MAX_FILETIME as i64 + 1), None);
        assert_eq!(safe_filetime(i64::MAX), None);
    }

    #[test]
    fn safe_filetime_passes_normal_value_through() {
        assert_eq!(safe_filetime(1), Some(1));
        assert_eq!(
            safe_filetime(132_000_000_000_000_000),
            Some(132_000_000_000_000_000)
        );
        assert_eq!(safe_filetime(MAX_FILETIME as i64), Some(MAX_FILETIME));
    }

    // ---- 9.1 菜单转义与截断 ----

    #[test]
    fn menu_esc_doubles_ampersand() {
        assert_eq!(menu_esc("B&O"), "B&&O");
        assert_eq!(menu_esc("耳机"), "耳机");
    }

    #[test]
    fn truncate_happens_before_escape_so_no_pair_is_split() {
        // 截断到 3 字符得 "AB&"，转义后必须是 "AB&&"
        // （若先转义再截断，会得到劈开成对符号的 "AB&"）
        let truncated = truncate_chars("AB&CD", 3);
        assert_eq!(truncated, "AB&");
        assert_eq!(menu_esc(truncated), "AB&&");
    }

    #[test]
    fn truncate_chars_counts_characters_not_bytes() {
        let text = "耳机设备名称";
        assert_eq!(truncate_chars(text, 2), "耳机");
        assert_eq!(truncate_chars(text, 6), text);
        assert_eq!(truncate_chars(text, 99), text);
    }

    #[test]
    fn menu_text_truncates_then_escapes() {
        assert_eq!(menu_text("B&O"), "B&&O");
        assert_eq!(menu_text("AB&CD"), "AB&&CD");
        // 59 个汉字 + "&CD"：截断到 60 字符正好切在单个 & 之后，转义后必须是 "&&"
        // （若先转义再截断，那对符号会被劈成一条孤零零的 &）
        let long = format!("{}&CD", "长".repeat(59));
        let out = menu_text(&long);
        assert_eq!(out.chars().count(), 61);
        assert!(out.ends_with("&&"), "{out}");
    }
}
