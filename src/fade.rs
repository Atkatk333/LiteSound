//! 横幅淡出序列。
//!
//! 【纯】：不引用任何 Windows API。

/// 每个淡出定时器周期的 alpha 递减量。定时器周期 40 ms，起始 alpha 240。
pub const FADE_STEP: u8 = 24;

/// 起始 alpha（规格 4.3）。
pub const FADE_START: u8 = 240;

/// 下一步的 alpha。
///
/// 唯一不变式：**任何输入都不得返回比输入更大的值**。淡出途中把 alpha 调回满值会让
/// 即将消失的横幅闪回全亮（坑 #19），所以这里只能是单调变暗，没有第二条路径。
///
/// 用 `saturating_sub` 而不是普通减法：alpha 小于步长时（例如 10）普通减法在 u8 上
/// 会回绕成 246，正好就是"闪回全亮"那个故障。
pub fn next_fade(alpha: u8) -> u8 {
    alpha.saturating_sub(FADE_STEP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fades_from_start_to_zero_within_40_steps() {
        let mut alpha = FADE_START;
        let mut prev = alpha;
        for step in 0..40 {
            alpha = next_fade(alpha);
            assert!(
                alpha <= prev,
                "第 {} 步出现变亮: {} -> {}",
                step,
                prev,
                alpha
            );
            prev = alpha;
        }
        assert_eq!(alpha, 0, "40 步内必须收敛到 0");
    }

    #[test]
    fn is_monotonic_non_increasing_for_every_possible_input() {
        for a in 0..=u8::MAX {
            assert!(next_fade(a) <= a, "alpha {} 被变亮了", a);
        }
    }

    #[test]
    fn steps_by_exactly_24() {
        assert_eq!(next_fade(240), 216);
        assert_eq!(next_fade(216), 192);
        assert_eq!(next_fade(48), 24);
    }

    #[test]
    fn clamps_to_zero_instead_of_wrapping() {
        assert_eq!(next_fade(24), 0);
        assert_eq!(next_fade(23), 0);
        assert_eq!(next_fade(1), 0);
        assert_eq!(next_fade(0), 0);
    }

    #[test]
    fn start_reaches_zero_in_exactly_ten_steps() {
        let mut alpha = FADE_START;
        let mut steps = 0;
        while alpha != 0 {
            alpha = next_fade(alpha);
            steps += 1;
        }
        assert_eq!(steps, 10);
    }
}
