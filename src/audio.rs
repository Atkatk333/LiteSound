//! Core Audio 端点枚举与属性读取。
//!
//! 本模块是唯一直接调 Windows Audio COM 的地方，上层只看得到 [`Entry`] 与 [`RawProps`]。
//!
//! 释放策略（规格 6.7 要求 RAII 与"统一出口"二选一）：这里选 **RAII**。windows crate 的
//! COM 接口包装在 `Drop` 里调 `Release`，所以 `IMMDeviceEnumerator` / `IMMDeviceCollection` /
//! `IMMDevice` / `IPropertyStore` 的任何一条提前 return 分支都会自动释放，不需要手写出口。
//! 只有两处**不归 Drop 管**，必须自己还：`GetId` 返回的 PWSTR 和 `PROPVARIANT` 里的字符串。
//!
//! COM 走查只有一份（[`walk`]），`enumerate` 与 `props` 是它的两种视图：前者做合法性折算
//! 供菜单/`--list` 用，后者保留原始值供 `--props` 排查命名问题。

use crate::history::{self, SwitchOutcome};
use crate::model::{bluetooth_from_icon_path, Entry, SortKind};
use crate::naming::safe_filetime;
use core::ffi::c_void;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use windows::core::{IUnknown, Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    eConsole, eMultimedia, eRender, ERole, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Variant::{VARENUM, VT_FILETIME, VT_I4, VT_LPWSTR, VT_UI4, VT_VECTOR};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

/// 状态掩码：只要 active（菜单 / 横幅 / tooltip 用）。
pub const ACTIVE: u32 = 0x1;

/// 状态掩码：四类都要（`--list` / `--props` 用）。
pub const ALL_STATES: u32 = 0xF;

// 规格 6.3 的四个属性键。fmtid 与 pid 必须逐字一致；这里直接写死而不是引
// `Win32_Devices_Properties`，是为了不为了四个常量再开一个 feature。

/// 设备类别（如"耳机""扬声器"），字符串。
const PKEY_CATEGORY: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 2,
};

/// 设备接口描述（如"蓝牙耳机A"），字符串。
const PKEY_INTERFACE_DESC: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xb3f8fa53_0004_438e_9003_51a46e139bfc),
    pid: 6,
};

/// 安装时间，FILETIME。
const PKEY_INSTALL_DATE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x83da6326_97a6_4088_9453_a1923f573b29),
    pid: 100,
};

/// 接入时间（**未公开**属性），FILETIME。实测 16 台里只有 4 台带它，缺失是多数路径。
const PKEY_ARRIVE_DATE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x194ef948_7cdb_403e_9f47_19418f7b24fd),
    pid: 2,
};

/// 端点形态因子（`PKEY_AudioEndpoint_FormFactor`），`VT_UI4`：1 = 扬声器、3 = 耳机、
/// 5 = 头戴耳麦、9 = HDMI、10 = 未知。
///
/// 规格 3.7 用它区分托盘图标的"音响 / 耳机"两类。**刻意不用类别文本**（"扬声器"/"Speakers"）：
/// 文本随系统语言变，数值不随——换机器、换语言、换耳机都得到同一结论。
const PKEY_FORM_FACTOR: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 0,
};

/// 端点形态因子的**上游**判据：驱动声明的 KS pin category GUID，字符串形式。
///
/// 加它只为 `--props` 取证：它是数值型 FormFactor 被推导出来之前那个原始值，两者逐台一比
/// 才知道"设备被分成音响还是耳机"这件事是固件写死的、还是 AEB 推的。交付路径不读它。
const PKEY_JACK_SUBTYPE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 8,
};

/// 系统给这台端点配的图标路径（形如 `%windir%\system32\mmres.dll,-3011`）。
///
/// 类型是字符串**数组**而不是单串，所以读它走 [`read_string_list`]。它的索引号是微软自己的
/// UI 用来选图形的依据，也是"蓝牙专用图标能不能白嫖"这个问题的答案所在。
const PKEY_ICON_PATH: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x259abffc_50a7_47ce_af08_68c9a7d73366),
    pid: 12,
};

/// `VT_VECTOR | VT_LPWSTR`：多重字符串变体的 VARENUM 值。
///
/// crate 只提供 `VT_VECTOR`(4096) 与 `VT_LPWSTR`(31) 两个基值，没有组合常量。
const VT_VECTOR_LPWSTR: VARENUM = VARENUM(VT_VECTOR.0 | VT_LPWSTR.0);

/// 惰性 COM 初始化状态：0 = 尚未尝试，1 = 可用，2 = 不可用。
static COM_STATE: AtomicU8 = AtomicU8::new(0);
static COM_ONCE: Once = Once::new();

/// 一次 COM 走查的原始结果，不做任何合法性折算。
///
/// 刻意保留"键不存在"与"值是空/ 0"的区别：折算成 [`Entry`] 之后这个区别就没了，
/// 而 `--props` 存在的全部意义正是把它看出来（0 时间戳会把设备无声顶到组内第一）。
#[derive(Debug, Clone)]
pub struct RawProps {
    pub id: String,
    pub state: u32,
    /// `None` = 属性库里没有这个键，或类型不是字符串。
    pub category: Option<String>,
    pub desc: Option<String>,
    /// 原始 FILETIME 计数，未经合法性判断。
    pub install: Option<i64>,
    pub arrive: Option<i64>,
    /// `PKEY_AudioEndpoint_FormFactor` 的原始数值，未经"这是哪一类"的折算。
    pub form_factor: Option<u32>,
    /// 驱动声明的 KS pin category GUID 字符串（FormFactor 的上游判据）。
    pub jack_subtype: Option<String>,
    /// 系统给这台配的图标路径（string list，多项已用 ` | ` 拼好）。
    pub icon_path: Option<String>,
    /// 属性库是否打得开。false 时上面那些值必然全是 `None`。
    pub store_ok: bool,
}

/// 惰性初始化 COM（坑 #13：启动阶段急切创建会在音频服务未就绪时先于诊断分支抛异常）。
///
/// 只初始化一次、且**不**配对 `CoUninitialize`：枚举是反复调用的热路径，套间一旦退掉，
/// 后续调用都得重来；进程退出时随进程回收即可。
///
/// `RPC_E_CHANGED_MODE` 算成功——它表示本线程已用别的套间模式初始化过 COM，
/// 音频调用照样能用，把它当失败会让"托盘先初始化过 COM"这种正常情况直接失去枚举能力。
fn ensure_com() -> bool {
    COM_ONCE.call_once(|| {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let ok = hr.is_ok() || hr == RPC_E_CHANGED_MODE;
        COM_STATE.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
    });
    COM_STATE.load(Ordering::Relaxed) == 1
}

/// 已报过"属性库打不开"的设备 ID。
///
/// 坑 #12：枚举是热路径，不去重会把日志写爆。按 ID 记，每台**一辈子只报一次**。
static STORE_FAILURES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn report_store_failure(id: &str) {
    let seen = STORE_FAILURES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
    if seen.insert(id.to_string()) {
        crate::diag::log(&format!("属性库打不开，保留裸 ID 作为描述: {id}"));
    }
}

/// 读原始终端属性。
///
/// `state_mask` **直接下传**给 `EnumAudioEndpoints`，不在这里过滤（规格 6.4）：
/// 菜单只要 3 台 active 时，不该为了另外 7 台各开一次属性库——那是一次跨进第三方
/// 音频属性插件的调用。
pub fn props(state_mask: u32) -> Vec<RawProps> {
    if !ensure_com() {
        crate::diag::log("COM 初始化失败，无法枚举端点");
        return Vec::new();
    }
    // SAFETY: 上面已确认本线程 COM 可用；COM 对象全部由 RAII 释放，见模块头注释。
    unsafe { walk(state_mask) }
}

/// 枚举渲染端点，折算成上层用的模型。
pub fn enumerate(state_mask: u32) -> Vec<Entry> {
    props(state_mask).into_iter().map(entry_from).collect()
}

/// 折算成 [`Entry`]：字符串缺失给空串，FILETIME 缺失与非法都落到 `None`。
///
/// 属性库整个打不开时**不丢设备**（坑 #12）：丢掉的话它既不进菜单也不进 `--list`，
/// 零提示，比多一行裸 ID 难查得多——所以这里用裸 ID 顶上描述位。
fn entry_from(raw: RawProps) -> Entry {
    let desc = match (raw.store_ok, raw.desc) {
        (true, d) => d.unwrap_or_default(),
        (false, _) => raw.id.clone(),
    };
    // 蓝牙判定要在 `raw.icon_path` 被丢弃前先算：它是这次属性走查里唯一带连接方式的线索，
    // 而 `Entry` 只留一个 bool，不把整条路径带到上层——菜单只关心"要不要挂符号"。
    let bluetooth = bluetooth_from_icon_path(raw.icon_path.as_deref());
    let mut entry = Entry {
        id: raw.id,
        state: raw.state,
        category: raw.category.unwrap_or_default(),
        desc,
        install: raw.install.and_then(safe_filetime),
        arrive: raw.arrive.and_then(safe_filetime),
        rank: 0,
        total: 0,
        name: String::new(),
        sort_kind: SortKind::Install,
        bluetooth,
    };
    entry.sort_kind = entry.sort_kind();
    entry
}

/// 当前默认渲染端点（`eConsole` 角色）的 ID。
///
/// 只读 `eConsole`：切换时会同时设 `eConsole` 与 `eMultimedia`（规格 6.5），所以两者
/// 一般一致；用 console 是因为它才是 Windows 音量浮窗显示的那个。
pub fn default_id() -> Option<String> {
    if !ensure_com() {
        return None;
    }
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let raw = device.GetId().ok()?;
        let text = raw.to_string().unwrap_or_default();
        CoTaskMemFree(Some(raw.0 as *const c_void));
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }
}

/// 取一个端点枚举器实例——托盘注册设备变更通知要用它。
///
/// 单独开放这一处，是为了让 `tray` 不必自己碰 `CoCreateInstance`：音频 COM 对象的创建
/// 仍然只发生在本模块里，`tray` 只拿现成的接口去注册/注销回调。
pub fn enumerator() -> Option<IMMDeviceEnumerator> {
    if !ensure_com() {
        crate::diag::log("COM 初始化失败，无法创建端点枚举器");
        return None;
    }
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL).ok() }
}

/// 当前默认渲染端点的**形态因子**（规格 3.7 的托盘图标判据）。
///
/// 只读默认那台：图标跟的就是默认设备，没必要为了它把全部端点枚举一遍。取不到（驱动没写
/// 这个属性、属性库打不开）就返回 `None`，由 `tray` 落到"音响"那一类——不做二义性判断。
pub fn default_form_factor() -> Option<u32> {
    if !ensure_com() {
        return None;
    }
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        read_u32(
            &device.OpenPropertyStore(STGM_READ).ok()?,
            &PKEY_FORM_FACTOR,
        )
    }
}

/// IPolicyConfig 的 IID。**非公开接口**：SDK 头文件里没有它（`audiopolicy.h` 只有
/// `IAud*Polic*`），`windows` crate 全库也搜不到，只能手写。
const IID_IPOLICYCONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

/// PolicyConfigClient 的 CLSID。
const CLSID_POLICYCONFIGCLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// `IPolicyConfig` 的 vtable 前缀。
///
/// 只声明到 `SetDefaultEndpoint` 为止——它是第 11 个方法，落在不算 IUnknown 那三格的
/// 第 13 格。中间 10 个方法（GetMixFormat … SetPropertyValue）本项目从不调用，用
/// `*const c_void` 占位即可：vtable 布局只要求每个槽位占一个指针大小。
///
/// **这个结构体是槽序的唯一载体**：少一格、多一格都会调到别的方法上，而且 HRESULT
/// 照样可能是成功——不会有任何报错，只是默认设备没换。改它之前先对 6.1 数一遍。
#[repr(C)]
struct PolicyConfigVtbl {
    _query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    _add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    _unused: [*const c_void; 10],
    set_default_endpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
}

/// 把默认渲染端点切到 `id`。
///
/// 写路径与判定分开：本函数只负责"写 + 回读 + 判定"，**不碰"上一台"记账**——
/// 记账要等调用方确认成功之后再动（`history` 模块头注释里的那条前提）。
pub fn set_default(id: &str) -> SwitchOutcome {
    if !ensure_com() {
        crate::diag::log("COM 初始化失败，无法切换默认端点");
        return SwitchOutcome::Failed;
    }

    let current = default_id();
    // 规格 6.5 首条：目标 == 当前时**先判再动，一次写入都不发**。
    // 这里用的就是 decide 的首条判据，只是提前到写之前。
    if history::same_id(current.as_deref(), Some(id)) {
        return SwitchOutcome::AlreadyCurrent;
    }

    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    let hr_ok = unsafe { apply_roles(PCWSTR::from_raw(wide.as_ptr())) };

    // 回读拿不到时重试一次：刚写成功后的一次瞬时 RPC 失败不该直接判成切换失败。
    let readback = default_id().or_else(default_id);

    history::decide(id, current.as_deref(), hr_ok, readback.as_deref())
}

/// 对同一 ID 依次设 `eConsole` 与 `eMultimedia`，两个 HRESULT 都收下来。
///
/// **不设 `eCommunications`（角色 2）**——这是刻意的边界：按通信角色取设备的程序
/// （Teams、YY 等）输出不跟随切换。文档要写明，免得被当 bug 报。
///
/// 两次调用都要发出去，不能用 `&&` 短路：第一条失败不代表第二条没生效，
/// 短掉会让"console 成功、multimedia 失败"这种半成功状态在日志里看不出来。
unsafe fn apply_roles(id: PCWSTR) -> bool {
    let unknown: IUnknown =
        match unsafe { CoCreateInstance(&CLSID_POLICYCONFIGCLIENT, None::<&IUnknown>, CLSCTX_ALL) }
        {
            Ok(u) => u,
            Err(_) => {
                crate::diag::log("CoCreateInstance(PolicyConfigClient) 失败");
                return false;
            }
        };

    let mut raw: *mut c_void = core::ptr::null_mut();
    if unsafe { unknown.query(&IID_IPOLICYCONFIG, &mut raw) }.is_err() || raw.is_null() {
        crate::diag::log("QueryInterface(IPolicyConfig) 失败");
        return false;
    }

    // SAFETY: raw 是上面刚 QI 到的 IPolicyConfig 指针，vtbl 与 raw 在本函数内都有效；
    // 引用计数由下面显式的 Release 归还（raw 不归 windows crate 的 Drop 管）。
    let vtbl = unsafe { *(raw as *const *const PolicyConfigVtbl) };
    let console = unsafe { ((*vtbl).set_default_endpoint)(raw, id, eConsole) };
    let multimedia = unsafe { ((*vtbl).set_default_endpoint)(raw, id, eMultimedia) };
    unsafe { ((*vtbl).release)(raw) };

    if console.is_err() || multimedia.is_err() {
        crate::diag::log(&format!(
            "SetDefaultEndpoint 返回 console=0x{:08X} multimedia=0x{:08X}",
            console.0, multimedia.0
        ));
    }
    console.is_ok() && multimedia.is_ok()
}

unsafe fn walk(state_mask: u32) -> Vec<RawProps> {
    let mut found = Vec::new();

    let enumerator: IMMDeviceEnumerator =
        match unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL) } {
            Ok(e) => e,
            Err(_) => {
                crate::diag::log("CoCreateInstance(MMDeviceEnumerator) 失败");
                return found;
            }
        };

    let collection =
        match unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE(state_mask)) } {
            Ok(c) => c,
            Err(_) => {
                crate::diag::log("EnumAudioEndpoints 失败");
                return found;
            }
        };

    let count = match unsafe { collection.GetCount() } {
        Ok(n) => n,
        Err(_) => {
            crate::diag::log("IMMDeviceCollection::GetCount 失败");
            return found;
        }
    };

    for index in 0..count {
        let device = match unsafe { collection.Item(index) } {
            Ok(d) => d,
            Err(_) => continue,
        };

        let id = match unsafe { device.GetId() } {
            Ok(raw) => {
                let text = unsafe { raw.to_string() }.unwrap_or_default();
                // GetId 给的是 CoTaskMem 分配的内存，Drop 不管它
                unsafe { CoTaskMemFree(Some(raw.0 as *const c_void)) };
                text
            }
            Err(_) => continue,
        };
        if id.is_empty() {
            continue;
        }

        let state = unsafe { device.GetState() }.map(|s| s.0).unwrap_or(0);

        let (category, desc, install, arrive, form_factor, jack_subtype, icon_path, store_ok) =
            match unsafe { device.OpenPropertyStore(STGM_READ) } {
                Ok(store) => {
                    let category = unsafe { read_string(&store, &PKEY_CATEGORY) };
                    let desc = unsafe { read_string(&store, &PKEY_INTERFACE_DESC) };
                    let install = unsafe { read_filetime(&store, &PKEY_INSTALL_DATE) };
                    let arrive = unsafe { read_filetime(&store, &PKEY_ARRIVE_DATE) };
                    let form_factor = unsafe { read_u32(&store, &PKEY_FORM_FACTOR) };
                    let jack_subtype = unsafe { read_string(&store, &PKEY_JACK_SUBTYPE) };
                    let icon_path = unsafe { read_string_list(&store, &PKEY_ICON_PATH) };
                    (
                        category,
                        desc,
                        install,
                        arrive,
                        form_factor,
                        jack_subtype,
                        icon_path,
                        true,
                    )
                }
                Err(_) => {
                    report_store_failure(&id);
                    (None, None, None, None, None, None, None, false)
                }
            };

        found.push(RawProps {
            id,
            state,
            category,
            desc,
            install,
            arrive,
            form_factor,
            jack_subtype,
            icon_path,
            store_ok,
        });
    }

    found
}

/// 读一个字符串属性；键不存在或类型不是字符串一律给 `None`。
///
/// 类型必须自己判：`GetValue` 对不存在的键可能成功返回一个 vt 不是 `VT_LPWSTR` 的
/// 变体，直接按 pwszVal 解读会读到垃圾指针。
unsafe fn read_string(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    let mut var = unsafe { store.GetValue(key) }.ok()?;
    let text = unsafe {
        let inner = &var.Anonymous.Anonymous;
        if inner.vt == VT_LPWSTR && !inner.Anonymous.pwszVal.is_null() {
            inner.Anonymous.pwszVal.to_string().ok()
        } else {
            None
        }
    };
    // PROPVARIANT 里的字符串是 CoTaskMem 分配的，windows crate 不会替我们释放
    let _ = unsafe { PropVariantClear(&mut var) };
    text
}

/// 读一个"字符串数组"属性，把各项用 ` | ` 拼起来；只为 `--props` 取证存在。
///
/// `PKEY_DeviceClass_IconPath` 的声明类型是 string list，实际驱动可能写成单个
/// `VT_LPWSTR`、也可能写成 `VT_VECTOR|VT_LPWSTR`，两种都得认：只写一种的话，另一类的
/// 设备会显示成"读不到"，而我们会把"读不到"误当成"这台没有图标信息"。
unsafe fn read_string_list(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    let mut var = unsafe { store.GetValue(key) }.ok()?;
    let text = unsafe {
        let inner = &var.Anonymous.Anonymous;
        if inner.vt == VT_LPWSTR && !inner.Anonymous.pwszVal.is_null() {
            inner.Anonymous.pwszVal.to_string().ok()
        } else if inner.vt == VT_VECTOR_LPWSTR {
            // PROPVARIANT 的 vector 变体是 CALPWSTR（cElems + pElems），不是 SAFEARRAY
            let vec = &inner.Anonymous.calpwstr;
            if vec.pElems.is_null() {
                None
            } else {
                (0..vec.cElems as usize)
                    .filter_map(|i| (*vec.pElems.add(i)).to_string().ok())
                    .reduce(|a, b| format!("{a} | {b}"))
            }
        } else {
            None
        }
    };
    let _ = unsafe { PropVariantClear(&mut var) };
    text
}

/// 读一个 FILETIME 属性的原始计数；键不存在或类型不是 FILETIME 一律给 `None`。
///
/// 这里返回**原始 i64**，合法性与上界交给 `naming::safe_filetime` 判——0 值和越界值
/// 都得在那里被挡掉（0 会变成 1601-01-01，一个会把设备无声顶到组内第一的排序键）。
unsafe fn read_filetime(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<i64> {
    let mut var = unsafe { store.GetValue(key) }.ok()?;
    let ticks = unsafe {
        let inner = &var.Anonymous.Anonymous;
        if inner.vt == VT_FILETIME {
            let ft = inner.Anonymous.filetime;
            Some((((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64) as i64)
        } else {
            None
        }
    };
    let _ = unsafe { PropVariantClear(&mut var) };
    ticks
}

/// 读一个 32 位属性；键不存在或类型不符一律给 `None`。
///
/// 顺带认 `VT_I4`：同一个属性在个别驱动上写成有符号型，不该因此被判成"没有"。
unsafe fn read_u32(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<u32> {
    let mut var = unsafe { store.GetValue(key) }.ok()?;
    let value = unsafe {
        let inner = &var.Anonymous.Anonymous;
        match inner.vt {
            VT_UI4 => Some(inner.Anonymous.ulVal),
            VT_I4 => Some(inner.Anonymous.lVal as u32),
            _ => None,
        }
    };
    let _ = unsafe { PropVariantClear(&mut var) };
    value
}
