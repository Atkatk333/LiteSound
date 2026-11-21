//! 横幅几何计算。
//!
//! 【纯】：不引用任何 Windows API，工作区尺寸与 dpi 缩放全部以参数传入。
//!
//! 所有尺寸都乘 `dpi_scale`：60 个汉字在 150% 缩放下就是 1700+ px，任何"按字符数兜底"
//! 的想法都兜不住，所以夹取一律**按像素**做。

/// 内边距基准（乘 dpi_scale）。
const PAD_BASE: f64 = 18.0;
/// 距工作区右下角的边距基准（乘 dpi_scale）。
const MARGIN_BASE: f64 = 24.0;
/// 圆角半径基准（乘 dpi_scale）。
const RADIUS_BASE: f64 = 12.0;
/// 最小可用文字宽（8 个 16 px 字符宽）。
const MIN_TEXT_W: i32 = 8 * 16;
/// 最小可用文字高。
const MIN_TEXT_H: i32 = 16;

/// 横幅矩形（工作区坐标系；调用方负责加上工作区原点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BannerRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub radius: i32,
}

fn scaled(base: f64, dpi_scale: f64) -> i32 {
    (base * dpi_scale).round() as i32
}

/// 工作区是否放得下最小可用横幅。
///
/// 放不下时**不弹**并记日志：弹出屏幕外的横幅与没弹是同一件事，但只有前者会被记录。
///
/// 下限照规格 4.3：宽 > 2×内边距 + 8×16，高 > 2×内边距 + 16。
pub fn banner_fits(work_w: i32, work_h: i32, dpi_scale: f64) -> bool {
    let pad = scaled(PAD_BASE, dpi_scale);
    work_w > 2 * pad + MIN_TEXT_W && work_h > 2 * pad + MIN_TEXT_H
}

/// 计算横幅矩形。
///
/// - 宽 = 文字宽 + 2×内边距，**上限 = 工作区宽 × 3/5**（超出则夹取，按像素夹）
/// - 高 = 文字高 + 2×内边距
/// - 右下边距 = 24 × dpi_scale
///
/// `text_w` / `text_h` 必须是**将要绘制的那一串**的测量结果（先按 60 字符截断再测量），
/// 否则窗口会按长文字开、按短文字画，裁切自检也永远不响（坑 #4）。
pub fn banner_geometry(
    text_w: i32,
    text_h: i32,
    work_w: i32,
    work_h: i32,
    dpi_scale: f64,
) -> BannerRect {
    let pad = scaled(PAD_BASE, dpi_scale);
    let margin = scaled(MARGIN_BASE, dpi_scale);

    let max_width = work_w * 3 / 5;
    let width = (text_w + 2 * pad).min(max_width);
    let height = text_h + 2 * pad;

    BannerRect {
        x: work_w - margin - width,
        y: work_h - margin - height,
        width,
        height,
        radius: scaled(RADIUS_BASE, dpi_scale),
    }
}

/// 目标几何相对窗口当前矩形要提交的变化（规格 4.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryChange {
    /// 位置与尺寸都没变。画面仍会照常提交（文字与 alpha 每回都变）
    None,
    /// 只有位置变了：新位置由每次画面提交带走，不需要额外动作
    Moved,
    /// 尺寸变了：除位置与画面外，还要重设圆角区域、重建位图
    Resized,
}

/// 比较窗口**当前**矩形与**目标**矩形，两者都用屏幕坐标、形如 `[左, 上, 宽, 高]`。
///
/// 位置与尺寸必须**分开判**：位置变了而尺寸没变（换显示器、工作区变化）时，横幅仍必须挪到
/// 新位置。只比尺寸的"没变就短路"会把这种情况当成没变化，横幅就留在旧位置。
pub fn geometry_change(current: [i32; 4], target: [i32; 4]) -> GeometryChange {
    if current[2] != target[2] || current[3] != target[3] {
        GeometryChange::Resized
    } else if current[0] != target[0] || current[1] != target[1] {
        GeometryChange::Moved
    } else {
        GeometryChange::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 9.3 适配判定 ----

    #[test]
    fn tiny_100px_workarea_does_not_fit() {
        assert!(!banner_fits(100, 100, 1.0));
    }

    #[test]
    fn normal_workareas_fit() {
        assert!(banner_fits(2560, 1440, 1.0));
        assert!(banner_fits(1024, 768, 1.0));
    }

    #[test]
    fn insufficient_height_does_not_fit() {
        assert!(!banner_fits(2560, 40, 1.0));
        // 高度恰好等于下限也不算放得下（判据是严格大于）
        assert!(!banner_fits(2560, 2 * 18 + MIN_TEXT_H, 1.0));
    }

    #[test]
    fn insufficient_width_does_not_fit() {
        assert!(!banner_fits(2 * 18 + MIN_TEXT_W, 1440, 1.0));
        assert!(banner_fits(2 * 18 + MIN_TEXT_W + 1, 1440, 1.0));
    }

    #[test]
    fn scaling_makes_small_workarea_fail_sooner() {
        // 100% 下内边距 18，高度下限 = 2*18+16 = 52；150% 下内边距 27，下限 = 70。
        // 高度 60 正好卡在两者之间：100% 够、150% 不够。
        assert!(banner_fits(1024, 60, 1.0));
        assert!(!banner_fits(1024, 60, 1.5));

        // 宽度同理：下限 164（100%）与 182（150%），宽度 170 卡在中间。
        assert!(banner_fits(170, 1440, 1.0));
        assert!(!banner_fits(170, 1440, 1.5));
    }

    // ---- 9.3 几何 ----

    #[test]
    fn geometry_adds_padding_on_both_sides() {
        let r = banner_geometry(200, 20, 2560, 1440, 1.0);
        assert_eq!(r.width, 200 + 2 * 18);
        assert_eq!(r.height, 20 + 2 * 18);
        assert_eq!(r.radius, 12);
    }

    #[test]
    fn geometry_is_anchored_to_bottom_right_with_24px_margin() {
        let r = banner_geometry(200, 20, 2560, 1440, 1.0);
        assert_eq!(r.x, 2560 - 24 - r.width);
        assert_eq!(r.y, 1440 - 24 - r.height);
        // 右下角贴合
        assert_eq!(r.x + r.width, 2560 - 24);
        assert_eq!(r.y + r.height, 1440 - 24);
    }

    #[test]
    fn width_is_clamped_at_three_fifths_of_workarea() {
        let r = banner_geometry(3000, 20, 2560, 1440, 1.0);
        assert_eq!(r.width, 2560 * 3 / 5);
        assert_eq!(r.x, 2560 - 24 - 2560 * 3 / 5);
    }

    #[test]
    fn clamped_width_still_fits_inside_workarea() {
        let r = banner_geometry(9000, 20, 1024, 768, 1.0);
        assert!(r.x >= 0);
        assert!(r.x + r.width <= 1024);
        assert!(r.y + r.height <= 768);
    }

    #[test]
    fn geometry_scales_with_dpi() {
        let r = banner_geometry(200, 20, 2560, 1440, 1.5);
        assert_eq!(r.width, 200 + 2 * 27);
        assert_eq!(r.height, 20 + 2 * 27);
        assert_eq!(r.radius, 18);
        assert_eq!(r.x, 2560 - 36 - r.width);
    }

    #[test]
    fn clamping_is_by_pixels_not_by_char_count() {
        // 同样的字符数在 150% 下宽度不同，夹取必须跟着像素走
        let at_100 = banner_geometry(1500, 20, 2560, 1440, 1.0);
        let at_150 = banner_geometry(2250, 30, 2560, 1440, 1.5);
        assert_eq!(at_100.width, 2560 * 3 / 5);
        assert_eq!(at_150.width, 2560 * 3 / 5);
        assert_ne!(at_150.height, at_100.height);
    }

    // ---- Step 26 追加修复：几何差异判定 ----

    #[test]
    fn identical_rects_are_no_change() {
        assert_eq!(
            geometry_change([100, 200, 330, 50], [100, 200, 330, 50]),
            GeometryChange::None
        );
    }

    #[test]
    fn position_change_with_same_size_still_reports_a_change() {
        // 换显示器 / 工作区变化：尺寸不变、位置变。位置随每次画面提交带走，必须报出来——
        // 只比尺寸的短路会把这种情况当成"没变化"漏掉，横幅就留在旧位置
        assert_eq!(
            geometry_change([100, 200, 330, 50], [140, 200, 330, 50]),
            GeometryChange::Moved
        );
        assert_eq!(
            geometry_change([100, 200, 330, 50], [100, 260, 330, 50]),
            GeometryChange::Moved
        );
    }

    #[test]
    fn size_change_wins_over_position_change() {
        assert_eq!(
            geometry_change([100, 200, 330, 50], [140, 260, 200, 50]),
            GeometryChange::Resized
        );
    }
}
