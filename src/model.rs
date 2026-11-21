//! 端点模型与排序键派生。
//!
//! 【纯】：不引用任何 Windows API，输入全部由调用方以参数/字段传入。

/// 排序键的**来源**，必须可观测——`--list` 要输出 arrive 还是 install。
///
/// 不写明来源，排查"顺序看着不对"时只能猜：它有两种成因完全不同的可能
/// （接入时间真的缺失 vs 接入时间读错了）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKind {
    Arrive,
    Install,
}

impl SortKind {
    /// `--list` 用的字面量，与规格 6.6 的 `arrive|install` 一致。
    pub fn as_str(self) -> &'static str {
        match self {
            SortKind::Arrive => "arrive",
            SortKind::Install => "install",
        }
    }
}

/// 排序键缺失哨兵：让"没有这个时间"排到组内最后，而不是排到 1601 年那台的前面。
pub const MISSING: u64 = u64::MAX;

/// 一台音频输出端点的渲染模型。
///
/// `rank` / `total` / `name` 由 `naming::number` 整批填充，构造时留 0 / 空串即可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    /// `IMMDevice::GetState` 的原始状态位，`--list` 的状态列直接用它。
    ///
    /// 必须**逐台记下来**而不是事后按 ID 再查一次：枚举用的状态掩码是调用方给的，
    /// 再查一次既多一趟 COM，又会和"这一批到底枚举了什么"脱钩。
    pub state: u32,
    pub category: String,
    pub desc: String,
    pub install: Option<u64>,
    pub arrive: Option<u64>,
    pub rank: u32,
    pub total: u32,
    pub name: String,
    pub sort_kind: SortKind,
    /// 这台端点是蓝牙音频设备（判据见 [`bluetooth_from_icon_path`]）。
    pub bluetooth: bool,
}

/// 系统给"蓝牙音频设备"用的 `mmres.dll` 图标索引。
///
/// 本机实测（2026-10-01，`--props` 全量 18 台）：多款蓝牙耳机与蓝牙音箱共 6 条蓝牙端点
/// **全部**是 `-3051`，而无一条非蓝牙端点落在它上面。
/// 之所以只收这一个数、而不是"从形状推蓝牙"：形状索引（音响 3010 / 耳机 3011）是形态，
/// 与连接方式无关，只有 3051 这一段是系统按"这是蓝牙音频"给的。
const BLUETOOTH_ICON_INDEX: u32 = 3051;

/// 从 `PKEY_DeviceClass_IconPath` 判这台设备是不是蓝牙。
///
/// 值形如 `%windir%\system32\mmres.dll,-3051`，取末尾那个带负号的资源号来比。**只在读得到
/// 且命中已知索引时才判真**：没这个属性、路径指向厂商自带 .ico、或将来换了别的索引号，
/// 一律判假——宁可菜单上少一个符号，也不给一台有线设备挂蓝牙标。
pub fn bluetooth_from_icon_path(path: Option<&str>) -> bool {
    let Some(tail) = path.and_then(|text| text.rsplit(',').next()) else {
        return false;
    };
    tail.trim()
        .strip_prefix('-')
        .and_then(|digits| digits.parse::<u32>().ok())
        == Some(BLUETOOTH_ICON_INDEX)
}

impl Entry {
    /// 排序键三元组：接入时间 → 安装时间 → ID。
    ///
    /// - 前两段缺失一律落 `MISSING`（u64::MAX），保证顺序总是确定
    /// - 第三段是 ID 的**序数比较**（字节序、区分大小写），不用本地化排序
    ///
    /// 注意"接入时间缺失"是**多数路径**不是例外路径：实测 16 台里只有 4 台带该属性。
    pub fn order_key(&self) -> (u64, u64, &str) {
        (
            self.arrive.unwrap_or(MISSING),
            self.install.unwrap_or(MISSING),
            self.id.as_str(),
        )
    }

    /// 本次排序实际用的是哪个键。`arrive` 为 `Some(MISSING)` 等同于缺失。
    pub fn sort_kind(&self) -> SortKind {
        match self.arrive {
            Some(v) if v != MISSING => SortKind::Arrive,
            _ => SortKind::Install,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, install: Option<u64>, arrive: Option<u64>) -> Entry {
        Entry {
            id: id.to_string(),
            state: 1,
            category: "耳机".to_string(),
            desc: "蓝牙耳机A".to_string(),
            install,
            arrive,
            rank: 0,
            total: 0,
            name: String::new(),
            sort_kind: SortKind::Install,
            bluetooth: false,
        }
    }

    #[test]
    fn bluetooth_hit_only_on_the_known_index() {
        assert!(bluetooth_from_icon_path(Some(
            "%windir%\\system32\\mmres.dll,-3051"
        )));
        // 形状索引与连接方式无关：音响、耳机、HDMI、未知都不该被认成蓝牙
        assert!(!bluetooth_from_icon_path(Some(
            "%windir%\\system32\\mmres.dll,-3010"
        )));
        assert!(!bluetooth_from_icon_path(Some(
            "%windir%\\system32\\mmres.dll,-3011"
        )));
        assert!(!bluetooth_from_icon_path(Some(
            "%windir%\\system32\\mmres.dll,-3017"
        )));
    }

    #[test]
    fn bluetooth_misses_silently_on_unparseable_input() {
        // 读不到属性、厂商自带 .ico、正数索引、非数字尾巴：一律判假而不是判"是"
        assert!(!bluetooth_from_icon_path(None));
        assert!(!bluetooth_from_icon_path(Some("")));
        assert!(!bluetooth_from_icon_path(Some(
            "C:\\Windows\\System32\\vendor.ico"
        )));
        assert!(!bluetooth_from_icon_path(Some("mmres.dll,3051")));
        assert!(!bluetooth_from_icon_path(Some("mmres.dll,-abcd")));
        // 尾巴里有空格也要认得：属性是 string list 时各项拼出来可能带空白
        assert!(bluetooth_from_icon_path(Some("mmres.dll, -3051")));
    }

    #[test]
    fn arrive_present_yields_arrive_kind() {
        let e = entry("{a}", Some(100), Some(200));
        assert_eq!(e.sort_kind(), SortKind::Arrive);
        assert_eq!(e.sort_kind().as_str(), "arrive");
    }

    #[test]
    fn arrive_absent_yields_install_kind() {
        let e = entry("{a}", Some(100), None);
        assert_eq!(e.sort_kind(), SortKind::Install);
        assert_eq!(e.sort_kind().as_str(), "install");
    }

    #[test]
    fn arrive_sentinel_counts_as_missing() {
        let e = entry("{a}", Some(100), Some(MISSING));
        assert_eq!(e.sort_kind(), SortKind::Install);
    }

    #[test]
    fn order_key_prefers_arrive_over_install() {
        let late_install_early_arrive = entry("{a}", Some(900), Some(100));
        let early_install_late_arrive = entry("{b}", Some(100), Some(900));
        assert!(late_install_early_arrive.order_key() < early_install_late_arrive.order_key());
    }

    #[test]
    fn order_key_falls_back_to_install_when_arrive_missing() {
        let e = entry("{a}", Some(100), None);
        assert_eq!(e.order_key().0, MISSING);
        assert_eq!(e.order_key().1, 100);
    }

    #[test]
    fn order_key_breaks_tie_by_id_ordinal_case_sensitive() {
        let upper = entry("{B}", Some(100), Some(100));
        let lower = entry("{a}", Some(100), Some(100));
        // 'B' (0x42) < 'a' (0x61)：字节序而不是字母序
        assert!(upper.order_key() < lower.order_key());
    }

    #[test]
    fn order_key_keeps_missing_last_within_same_install() {
        let known = entry("{a}", Some(100), Some(500));
        let missing = entry("{b}", Some(100), None);
        assert!(known.order_key() < missing.order_key());
    }
}
