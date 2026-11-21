//! "上一台"记账与切换判定。
//!
//! 【纯】：不引用任何 Windows API，输入全部以参数传入。
//!
//! 这两件事合在一个模块里，是因为它们共享同一条前提：**记账只在确认成功之后**。
//! 把"上一台"写成当前设备就把左键锁死了（切到自己 = 已是当前 = 什么都不做）。

/// ID 比对：忽略大小写。`None` 与任何值（包括另一个 `None`）都不相等。
///
/// 端点 ID 形如 `{0.0.0.0.00000000}.{guid}`，是纯 ASCII，因此用无分配的 ASCII 比对即可。
pub fn same_id(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

/// 对正"上一台"记账。
///
/// 返回值是**三态**，必须区分开：
/// - `None` —— 不动
/// - `Some(None)` —— 清空
/// - `Some(Some(x))` —— 改成 `x`
///
/// | 情形 | 结果 |
/// |---|---|
/// | 读不到当前默认 | 不动 |
/// | 外部换了设备（参照 ≠ 当前） | 改成**我们离开的那台**（参照） |
/// | 自己切的（参照 == 当前） | 不动（除非命中下一条） |
/// | 还没有参照 | 不动（除非命中下一条） |
/// | **"上一台" == 当前默认** | **清空**——此时左键退回菜单 |
///
/// 最后一条不是收尾补丁：状态文件完全可能是"上一台 == 当前默认"，不清掉的话左键永远
/// 只是弹菜单，且日志里一个字都没有。所以它在两个"不动"分支里都必须先被检查，
/// 否则"自己切的"这条早返回会把它整个绕过去。
///
/// 一处规格未覆盖的交叉：外部换设备**且** `last == current` 时，本实现按上表行序取
/// "改成参照"，不清空（参照 ≠ 当前，改完仍满足"上一台 ≠ 当前"这条不变式）。
pub fn next_last(
    last: Option<&str>,
    reference: Option<&str>,
    current: Option<&str>,
) -> Option<Option<String>> {
    let current = current?;

    match reference {
        Some(r) if !same_id(Some(r), Some(current)) => Some(Some(r.to_string())),
        _ => {
            if same_id(last, Some(current)) {
                Some(None)
            } else {
                None
            }
        }
    }
}

/// 切换结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchOutcome {
    AlreadyCurrent,
    Failed,
    Switched,
}

/// 切换判定。
///
/// | 条件 | 结果 |
/// |---|---|
/// | 目标 == 当前 | 已是当前（**先判再动**，一次写入都不发） |
/// | 返回值失败，或 回读 ≠ 目标 | 失败 |
/// | 否则 | 成功 |
///
/// 回读必须参与判定：音频端点构建器重启、端点在枚举之后消失，都能让"两个返回值都成功
/// 而默认设备其实没换"成立。回读为空（`None`）与回读不符同判失败。
///
/// `current` 为 `None`（切换前拿不到默认）不构成失败，只是"不可能是已是当前"，继续按
/// 返回值与回读判定。
pub fn decide(
    target: &str,
    current: Option<&str>,
    hr_ok: bool,
    readback: Option<&str>,
) -> SwitchOutcome {
    if same_id(Some(target), current) {
        return SwitchOutcome::AlreadyCurrent;
    }
    if !hr_ok || !same_id(Some(target), readback) {
        return SwitchOutcome::Failed;
    }
    SwitchOutcome::Switched
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 9.2 ID 比对 ----

    #[test]
    fn same_id_ignores_case() {
        assert!(same_id(Some("{ABC}"), Some("{abc}")));
        assert!(same_id(Some("{abc}"), Some("{AbC}")));
    }

    #[test]
    fn same_id_is_false_for_different_ids() {
        assert!(!same_id(Some("{a}"), Some("{b}")));
    }

    #[test]
    fn same_id_is_false_when_either_side_is_none() {
        assert!(!same_id(None, Some("{a}")));
        assert!(!same_id(Some("{a}"), None));
        // 规格原话是"None 与任何值不等"，因此 None 与 None 也不等
        assert!(!same_id(None, None));
    }

    #[test]
    fn empty_id_differs_from_non_empty() {
        assert!(!same_id(Some("{a}"), Some("")));
        assert!(!same_id(Some(""), Some("{a}")));
    }

    // ---- 9.2 记账 ----

    #[test]
    fn external_switch_sets_last_to_the_device_we_left() {
        // 参照 X ≠ 当前 Y：用户在外面从 X 切到了 Y，所以"上一台"= X
        let out = next_last(Some("{old}"), Some("{x}"), Some("{y}"));
        assert_eq!(out, Some(Some("{x}".to_string())));
    }

    #[test]
    fn own_switch_leaves_last_alone() {
        // 参照 == 当前：这是我们自己切的结果，回溯会把刚记好的账改坏
        let out = next_last(Some("{prev}"), Some("{y}"), Some("{y}"));
        assert_eq!(out, None);
    }

    #[test]
    fn no_reference_leaves_last_alone() {
        assert_eq!(next_last(Some("{prev}"), None, Some("{y}")), None);
    }

    #[test]
    fn unreadable_current_leaves_last_alone() {
        assert_eq!(next_last(Some("{prev}"), Some("{x}"), None), None);
        assert_eq!(next_last(None, None, None), None);
    }

    #[test]
    fn last_equal_to_current_is_cleared() {
        // 规格点名的场景：不清掉的话左键永远只是弹菜单，且日志里一个字都没有
        assert_eq!(next_last(Some("{y}"), Some("{y}"), Some("{y}")), Some(None));
    }

    #[test]
    fn last_equal_to_current_is_cleared_even_without_reference() {
        assert_eq!(next_last(Some("{y}"), None, Some("{y}")), Some(None));
    }

    #[test]
    fn last_comparison_to_current_ignores_case() {
        assert_eq!(next_last(Some("{Y}"), Some("{y}"), Some("{y}")), Some(None));
    }

    #[test]
    fn external_switch_wins_over_clear_when_both_match() {
        // 规格未覆盖的交叉：外部换设备且 last == current，按行序取"改成参照"
        let out = next_last(Some("{y}"), Some("{x}"), Some("{y}"));
        assert_eq!(out, Some(Some("{x}".to_string())));
    }

    #[test]
    fn no_last_at_all_is_not_clear() {
        assert_eq!(next_last(None, Some("{y}"), Some("{y}")), None);
    }

    // ---- 9.2 切换判定 ----

    #[test]
    fn decide_reports_already_current() {
        assert_eq!(
            decide("{a}", Some("{a}"), true, Some("{a}")),
            SwitchOutcome::AlreadyCurrent
        );
        // 先判再动：即使返回值是失败、回读为空，也判已是当前
        assert_eq!(
            decide("{A}", Some("{a}"), false, None),
            SwitchOutcome::AlreadyCurrent
        );
    }

    #[test]
    fn decide_fails_when_write_failed() {
        assert_eq!(
            decide("{b}", Some("{a}"), false, Some("{b}")),
            SwitchOutcome::Failed
        );
    }

    #[test]
    fn decide_fails_when_readback_differs() {
        assert_eq!(
            decide("{b}", Some("{a}"), true, Some("{c}")),
            SwitchOutcome::Failed
        );
    }

    #[test]
    fn decide_fails_when_readback_is_empty() {
        assert_eq!(
            decide("{b}", Some("{a}"), true, None),
            SwitchOutcome::Failed
        );
    }

    #[test]
    fn decide_switches_when_readback_matches() {
        assert_eq!(
            decide("{b}", Some("{a}"), true, Some("{b}")),
            SwitchOutcome::Switched
        );
        // 回读比对同样忽略大小写
        assert_eq!(
            decide("{B}", Some("{a}"), true, Some("{b}")),
            SwitchOutcome::Switched
        );
    }

    #[test]
    fn decide_proceeds_when_current_is_unreadable() {
        assert_eq!(
            decide("{b}", None, true, Some("{b}")),
            SwitchOutcome::Switched
        );
        assert_eq!(
            decide("{b}", None, false, Some("{b}")),
            SwitchOutcome::Failed
        );
    }
}
