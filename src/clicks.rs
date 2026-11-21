//! 托盘鼠标消息判定与去抖。
//!
//! 【纯】：不引用任何 Windows API。消息号与时间戳都由调用方传入——时间戳来自
//! `GetTickCount64`（**64 位**，32 位 tick 约 24.8 天回绕一次，回绕瞬间差值为负，
//! 此后首次点击被吞，坑 #7）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// 消息号直接写字面量，不引 windows crate：本模块是【纯】的，要能在任意平台跑单测。
//
// 只有"抬起"那几个会在生产代码里被判出来，其余几个是**刻意要被忽略**的消息号，它们只在
// 测试里用来驱动"按下 / 移动 / 双击不得写时间戳"这几条序列，因此只在测试配置下存在。
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONUP: u32 = 0x0208;
#[cfg(test)]
const WM_MOUSEMOVE: u32 = 0x0200;
#[cfg(test)]
const WM_LBUTTONDOWN: u32 = 0x0201;
#[cfg(test)]
const WM_LBUTTONDBLCLK: u32 = 0x0203;
#[cfg(test)]
const WM_RBUTTONDOWN: u32 = 0x0204;
#[cfg(test)]
const WM_MBUTTONDOWN: u32 = 0x0207;

/// 去抖窗口。视觉双击会被 shell 拆成 `抬起 + 双击 + 抬起`，去抖让它只算一次切换。
pub const DEBOUNCE_MS: u64 = 250;

/// 托盘点击语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// 忽略
    None,
    /// 切到"上一台"
    Toggle,
    /// 弹出设备菜单
    Menu,
}

/// 上一次**抬起**类动作的 tick。
///
/// 只有左键/中键/右键抬起会写它。移动、按下、双击一律不写——它们与抬起只差 0~1 ms，
/// 一旦记下时间戳，真正的抬起会被自己人吞掉（坑 #22）。
static LAST_UP: AtomicU64 = AtomicU64::new(0);

/// 菜单是否正在显示。
///
/// 菜单是模态的但仍会派发托盘消息，菜单开着时再点一次图标会**当场改设备**，而菜单项是
/// 打开瞬间的快照，松手后会按失效下标派发（坑 #8）。用一个标志挡掉。
static IN_MENU: AtomicBool = AtomicBool::new(false);

/// 由托盘窗口在菜单弹出/收起时调用。
pub fn set_menu(open: bool) {
    IN_MENU.store(open, Ordering::Relaxed);
}

/// 菜单关闭：放开重入标志，并把"关菜单的那一下"记进去抖。
///
/// 自绘菜单在 `WA_INACTIVE`（鼠标**按下**）时就收了，而托盘回调要到**抬起**才到；
/// 中间这点时间差足够让标志先放开、把"关菜单"误判成"切设备"。
pub fn close_menu(now_tick: u64) {
    LAST_UP.store(now_tick, Ordering::Relaxed);
    IN_MENU.store(false, Ordering::Relaxed);
}

/// 清空全部状态。正常流程只在测试里用；托盘退出路径也可用它保证不留残留。
pub fn reset() {
    LAST_UP.store(0, Ordering::Relaxed);
    IN_MENU.store(false, Ordering::Relaxed);
}

/// 判定一条托盘窗口消息该做什么。
///
/// 去抖窗口**跨动作类**（左/中/右键共用一个时间戳）：右键后 250 ms 内的左键会被丢弃。
/// 这是已知残留缺陷，规格明确要求保持现状，不修。
pub fn classify(msg: u32, now_tick: u64) -> TrayAction {
    let action = match msg {
        WM_LBUTTONUP | WM_MBUTTONUP => TrayAction::Toggle,
        WM_RBUTTONUP => TrayAction::Menu,
        // 移动 / 按下 / 双击等：既不产生动作，也绝不写时间戳
        _ => return TrayAction::None,
    };

    if IN_MENU.load(Ordering::Relaxed) {
        return TrayAction::None;
    }

    let last = LAST_UP.load(Ordering::Relaxed);
    if now_tick.wrapping_sub(last) < DEBOUNCE_MS {
        return TrayAction::None;
    }

    LAST_UP.store(now_tick, Ordering::Relaxed);
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// clicks 的状态是**进程级 static**，而 `cargo test` 默认多线程并行跑同一个二进制——
    /// 不加锁时各测试会互相清掉对方的时间戳，表现为"单独跑绿、整跑偶发红"。
    /// 用一个串行锁把每个用到状态的测试互斥开；中毒（别的测试 panic 过）也能继续用。
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 按"真机抓到的消息序列"驱动，返回每一步的判定结果。
    fn drive(seq: &[(u32, u64)]) -> Vec<TrayAction> {
        let _guard = serial();
        reset();
        seq.iter().map(|&(msg, tick)| classify(msg, tick)).collect()
    }

    fn count(actions: &[TrayAction], want: TrayAction) -> usize {
        actions.iter().filter(|a| **a == want).count()
    }

    // ---- 9.5 序列 1：左键按下 + 抬起（同毫秒）→ 只触发切换 ----

    #[test]
    fn left_down_and_up_in_same_ms_toggles_once() {
        let actions = drive(&[(WM_LBUTTONDOWN, 10_000), (WM_LBUTTONUP, 10_000)]);
        assert_eq!(actions, vec![TrayAction::None, TrayAction::Toggle]);
        assert_eq!(count(&actions, TrayAction::Toggle), 1);
    }

    // ---- 9.5 序列 2：右键按下 + 抬起（差 1 ms）→ 只触发菜单 ----

    #[test]
    fn right_down_and_up_one_ms_apart_opens_menu_once() {
        let actions = drive(&[(WM_RBUTTONDOWN, 10_000), (WM_RBUTTONUP, 10_001)]);
        assert_eq!(actions, vec![TrayAction::None, TrayAction::Menu]);
        assert_eq!(count(&actions, TrayAction::Menu), 1);
        assert_eq!(count(&actions, TrayAction::Toggle), 0);
    }

    // ---- 9.5 序列 3：视觉双击（按下, 抬起, 双击, 抬起）→ 只算一次切换 ----

    #[test]
    fn visual_double_click_counts_as_one_toggle() {
        let actions = drive(&[
            (WM_LBUTTONDOWN, 10_000),
            (WM_LBUTTONUP, 10_000),
            (WM_LBUTTONDBLCLK, 10_001),
            (WM_LBUTTONUP, 10_001),
        ]);
        assert_eq!(count(&actions, TrayAction::Toggle), 1);
        assert_eq!(actions[2], TrayAction::None);
        assert_eq!(actions[3], TrayAction::None);
    }

    // ---- 9.5 序列 4：50 次移动洪水后左键仍有效 ----

    #[test]
    fn left_click_still_works_after_move_flood() {
        let mut seq: Vec<(u32, u64)> = (0..50).map(|i| (WM_MOUSEMOVE, 10_000 + i)).collect();
        seq.push((WM_LBUTTONDOWN, 10_100));
        seq.push((WM_LBUTTONUP, 10_100));

        let actions = drive(&seq);
        assert_eq!(count(&actions, TrayAction::Toggle), 1);
        // 移动洪水本身一个动作都不产生
        assert_eq!(count(&actions, TrayAction::None), 51);
    }

    // ---- 9.5 序列 5：中键按下 + 抬起 → 触发切换 ----

    #[test]
    fn middle_down_and_up_toggles() {
        let actions = drive(&[(WM_MBUTTONDOWN, 10_000), (WM_MBUTTONUP, 10_001)]);
        assert_eq!(actions, vec![TrayAction::None, TrayAction::Toggle]);
    }

    // ---- 9.5 序列 6：两次间隔 400 ms 的独立点击 → 各算一次 ----

    #[test]
    fn two_clicks_400ms_apart_each_count() {
        let actions = drive(&[
            (WM_LBUTTONDOWN, 10_000),
            (WM_LBUTTONUP, 10_000),
            (WM_LBUTTONDOWN, 10_400),
            (WM_LBUTTONUP, 10_400),
        ]);
        assert_eq!(count(&actions, TrayAction::Toggle), 2);
    }

    // ---- 去抖边界 ----

    #[test]
    fn click_exactly_at_debounce_edge_counts() {
        let actions = drive(&[(WM_LBUTTONUP, 10_000), (WM_LBUTTONUP, 10_000 + DEBOUNCE_MS)]);
        assert_eq!(count(&actions, TrayAction::Toggle), 2);
    }

    #[test]
    fn click_one_ms_before_debounce_edge_is_swallowed() {
        let actions = drive(&[
            (WM_LBUTTONUP, 10_000),
            (WM_LBUTTONUP, 10_000 + DEBOUNCE_MS - 1),
        ]);
        assert_eq!(count(&actions, TrayAction::Toggle), 1);
    }

    // ---- 菜单重入 ----

    #[test]
    fn clicks_are_ignored_while_menu_is_open() {
        let _guard = serial();
        reset();
        set_menu(true);
        assert_eq!(classify(WM_LBUTTONUP, 10_000), TrayAction::None);
        assert_eq!(classify(WM_RBUTTONUP, 10_000), TrayAction::None);
        set_menu(false);
        assert_eq!(classify(WM_LBUTTONUP, 10_000), TrayAction::Toggle);
    }

    #[test]
    fn ignored_click_while_menu_open_does_not_poison_debounce() {
        let _guard = serial();
        reset();
        set_menu(true);
        assert_eq!(classify(WM_LBUTTONUP, 10_000), TrayAction::None);
        set_menu(false);
        // 菜单里那一下没有写时间戳，因此紧接着的真点击必须生效
        assert_eq!(classify(WM_LBUTTONUP, 10_001), TrayAction::Toggle);
    }

    #[test]
    fn closing_the_menu_swallows_the_click_that_closed_it() {
        let _guard = serial();
        reset();
        set_menu(true);
        // 关闭发生在鼠标按下，托盘回调在抬起：这一下必须被吞掉
        close_menu(10_000);
        assert_eq!(classify(WM_LBUTTONUP, 10_080), TrayAction::None);
        // 去抖窗口之外的点击照常生效
        assert_eq!(
            classify(WM_LBUTTONUP, 10_000 + DEBOUNCE_MS),
            TrayAction::Toggle
        );
    }

    #[test]
    fn reset_clears_both_states() {
        let _guard = serial();
        reset();
        set_menu(true);
        assert_eq!(classify(WM_LBUTTONUP, 10_000), TrayAction::None);
        reset();
        assert_eq!(classify(WM_LBUTTONUP, 10_001), TrayAction::Toggle);
    }
}
