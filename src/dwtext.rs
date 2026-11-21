//! 菜单文字的 DirectWrite 灰度覆盖率路径。
//!
//! 取代 GDI `DrawTextW` 的**分层窗口**渲染：`WS_EX_LAYERED` 交的是逐像素 alpha，DWM 会把
//! ClearType 的三通道相位补偿与未知壁纸合成，边缘一定出现彩边（见 `tray::menu_font` 的
//! 注释）。这里只用 `IDWriteGlyphRunAnalysis::CreateAlphaTexture(DWRITE_TEXTURE_ALIASED_1x1)`
//! 拿每像素 1 字节的覆盖率，再用前景/底色做通道相同的混合，输出天然是纯灰度。
//!
//! 调用链（全部是 `windows` crate 的绑定，无 Direct2D、无新依赖）：
//!
//! ```text
//! DWriteCreateFactory(IDWriteFactory, SHARED)
//!   → GetSystemFontCollection → FindFamilyName("Noto Sans SC Medium") / 回退 "Microsoft YaHei UI"
//!   → GetFirstMatchingFont(weight=500, stretch/style=normal) → CreateFontFace
//!   → GetMetrics（ascent/descent/designUnitsPerEm）
//! IDWriteFactory::CreateTextAnalyzer
//!   → AnalyzeScript（自实现的 IDWriteTextAnalysisSource/Sink）
//!   → GetGlyphs → GetGlyphPlacements（分数步进保留到最终 run）
//! IDWriteFactory2::CreateGlyphRunAnalysis
//!   (NATURAL_SYMMETRIC, MEASURING_NATURAL, GRID_FIT, GRAYSCALE, 整数像素基线)
//!   → GetAlphaTextureBounds(ALIASED_1x1) → CreateAlphaTexture → 混合进 32 位 BGRA DIB
//! ```
//!
//! 缓存只在**托盘线程**：COM 对象不具备跨线程可移植性，而菜单全部在托盘线程创建与绘制，
//! 用 `thread_local!` + `std::cell::OnceCell` 即可，不需要 `Send`，也不额外加锁。

use std::cell::{Cell, OnceCell};
use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};
use std::ptr;
use std::rc::Rc;

use windows::core::{implement, Error, Interface, OutRef, Result, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, RECT};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_TEXTURE_ALIASED_1x1, DWriteCreateFactory, IDWriteFactory, IDWriteFactory2,
    IDWriteFontCollection, IDWriteFontFace, IDWriteNumberSubstitution, IDWriteTextAnalysisSink,
    IDWriteTextAnalysisSink_Impl, IDWriteTextAnalysisSource, IDWriteTextAnalysisSource_Impl,
    IDWriteTextAnalyzer, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_GLYPH_OFFSET,
    DWRITE_GLYPH_RUN, DWRITE_GRID_FIT_MODE_ENABLED, DWRITE_LINE_BREAKPOINT,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_READING_DIRECTION,
    DWRITE_READING_DIRECTION_LEFT_TO_RIGHT, DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
    DWRITE_SCRIPT_ANALYSIS, DWRITE_SHAPING_GLYPH_PROPERTIES, DWRITE_SHAPING_TEXT_PROPERTIES,
    DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE, DWRITE_TYPOGRAPHIC_FEATURES,
};
use windows::Win32::Graphics::Gdi::{GetCurrentObject, GetObjectW, DIBSECTION, HDC, OBJ_BITMAP};
use windows::Win32::UI::HiDpi::GetDpiForSystem;

use crate::diag;

/// GDI 看到的字面名（按字重拆成独立族）。DirectWrite 的系统字体集合未必这么命名，
/// 所以下面还有 `MENU_FAMILY` 这一档。
const MENU_FACE: &str = "Noto Sans SC Medium";
/// DirectWrite 实际枚举到的族名（Noto Sans SC 的 500 档在族内，不单独成族）。
const MENU_FAMILY: &str = "Noto Sans SC";
/// 找不到首选字面时的回退。
const FALLBACK_FACE: &str = "Microsoft YaHei UI";
/// 与 `tray::MENU_FONT_WEIGHT` 同值：直接请求 500，不被族内匹配静默落成 400。
const MENU_WEIGHT: i32 = 500;

/// 整形用的语言。菜单内容为中文/拉丁，不需要额外的语种分支。
const LOCALE: [u16; 6] = [
    b'e' as u16,
    b'n' as u16,
    b'-' as u16,
    b'u' as u16,
    b's' as u16,
    0,
];

/// 一次初始化的全部 COM 对象。只在托盘线程存活（thread_local）。
#[derive(Clone)]
struct Engine {
    /// IDWriteFactory2：只有它的八参 `CreateGlyphRunAnalysis` 能显式要求
    /// `DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE`；基类七参版本在这台机器上只会产出
    /// ClearType 分析，`DWRITE_TEXTURE_ALIASED_1x1` 直接返回空覆盖图（已实测）。
    factory2: IDWriteFactory2,
    analyzer: IDWriteTextAnalyzer,
    face: IDWriteFontFace,
    metrics: DWRITE_FONT_METRICS,
    /// 物理像素 / DIP。菜单进程声明了 Per-Monitor V2，这个比例就是系统 DPI / 96。
    pixels_per_dip: f32,
}

thread_local! {
    /// 首次绘制时初始化；失败也缓存（`None`），避免每行都重试 DWrite 工厂。
    static ENGINE: OnceCell<Option<Engine>> = const { OnceCell::new() };
    /// 运行期失败的日志只写一条，不随每帧/每行重复。
    static RUNTIME_FAIL_LOGGED: Cell<bool> = const { Cell::new(false) };
}

/// 把字符串按 DirectWrite 的覆盖图路径画进 32 位 BGRA DIB。
///
/// * `dc` 必须选入一张 32 位自顶向下 DIB（`menu::Surface` 正是这么建的）。
/// * `rect` 是**文字矩形**（已经包含 `MENU_PAD_L`/标记列/`MENU_GAP` 与 `MENU_TEXT_RISE`）；
///   超出右边界的覆盖像素按像素裁掉，和 `DrawTextW` 的裁剪行为一致。
/// * `px` 是 `tray::menu_font` 返回的物理高度（即 GDI `lfHeight` 的绝对值 = em 尺寸）。
/// * 返回 `false` 表示初始化/本次绘制失败，调用方必须回退 GDI。
pub(crate) fn draw(
    dc: HDC,
    rect: &RECT,
    text: &[u16],
    px: i32,
    cell_px: i32,
    background: u32,
    foreground: u32,
) -> bool {
    if px <= 0 || cell_px <= 0 || text.is_empty() {
        return false;
    }
    // 调用方的文本以 UTF-16 + 结尾 0 的形式存放（与 DrawTextW 共用同一份 Vec）。
    let body_len = text
        .iter()
        .rposition(|unit| *unit != 0)
        .map_or(0, |i| i + 1);
    if body_len == 0 {
        // 空字符串在两条路径里都不可见，直接算成功，免得无谓地退回 GDI。
        return true;
    }

    ENGINE.with(|slot| {
        let engine = slot.get_or_init(init_engine);
        match engine {
            Some(engine) => match unsafe {
                draw_with(
                    engine, dc, rect, text, body_len, cell_px, background, foreground,
                )
            } {
                Ok(()) => true,
                Err(err) => {
                    if !RUNTIME_FAIL_LOGGED.with(Cell::get) {
                        RUNTIME_FAIL_LOGGED.with(|flag| flag.set(true));
                        diag::log(&format!("DirectWrite 菜单文字绘制失败，回退 GDI: {err:?}"));
                    }
                    false
                }
            },
            None => false,
        }
    })
}

fn fail(message: &str) -> Error {
    Error::new(E_FAIL, message)
}

fn init_engine() -> Option<Engine> {
    let factory = match unsafe { DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED) }
    {
        Ok(factory) => factory,
        Err(err) => {
            diag::log(&format!(
                "DirectWrite 工厂创建失败，菜单文字回退 GDI: {err:?}"
            ));
            return None;
        }
    };

    let factory2: IDWriteFactory2 = match factory.cast() {
        Ok(factory2) => factory2,
        Err(err) => {
            diag::log(&format!(
                "DirectWrite 工厂不支持灰度分析（IDWriteFactory2），菜单文字回退 GDI: {err:?}"
            ));
            return None;
        }
    };

    let mut collection: Option<IDWriteFontCollection> = None;
    if let Err(err) = unsafe { factory.GetSystemFontCollection(&mut collection, false) } {
        diag::log(&format!(
            "DirectWrite 系统字体集合不可用，菜单文字回退 GDI: {err:?}"
        ));
        return None;
    }
    let Some(collection) = collection else {
        diag::log("DirectWrite 系统字体集合为空，菜单文字回退 GDI");
        return None;
    };

    let face = match pick_face(&collection) {
        Some(picked) => picked,
        None => {
            diag::log("DirectWrite 找不到可用菜单字面，菜单文字回退 GDI");
            return None;
        }
    };
    let mut metrics = DWRITE_FONT_METRICS::default();
    unsafe { face.GetMetrics(&mut metrics) };
    if metrics.designUnitsPerEm == 0 {
        diag::log("DirectWrite 字体度量不可用（unitsPerEm=0），菜单文字回退 GDI");
        return None;
    }

    let analyzer = match unsafe { factory.CreateTextAnalyzer() } {
        Ok(analyzer) => analyzer,
        Err(err) => {
            diag::log(&format!(
                "DirectWrite 整形器创建失败，菜单文字回退 GDI: {err:?}"
            ));
            return None;
        }
    };

    let dpi = unsafe { GetDpiForSystem() };
    let pixels_per_dip = if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 };

    Some(Engine {
        factory2,
        analyzer,
        face,
        metrics,
        pixels_per_dip,
    })
}

/// 系统字体集合 → 首选/回退字面 → 字面对象。
///
/// `GetFirstMatchingFont` 的返回是族内"最接近请求字重"的那一档；回退时把实际字重写进
/// 日志，免得再出现"以为请求了 500、实际落到 400"的静默降级。
fn pick_face(collection: &IDWriteFontCollection) -> Option<IDWriteFontFace> {
    // DirectWrite 的系统字体集合在这台机器上把 Noto Sans SC 合并成一个族（500 在族内），
    // 而 GDI 看到的是按字重拆开的独立族。两个名字都试，最后才回退 YaHei UI。
    for family in [MENU_FACE, MENU_FAMILY, FALLBACK_FACE] {
        let mut name: Vec<u16> = family.encode_utf16().collect();
        name.push(0);
        let mut family_index = u32::MAX;
        let mut exists = windows::core::BOOL(0);
        let found = unsafe {
            collection.FindFamilyName(PCWSTR(name.as_ptr()), &mut family_index, &mut exists)
        };
        if found.is_err() || !exists.as_bool() || family_index == u32::MAX {
            continue;
        }
        let font_family = match unsafe { collection.GetFontFamily(family_index) } {
            Ok(family) => family,
            Err(_) => continue,
        };
        let font = match unsafe {
            font_family.GetFirstMatchingFont(
                DWRITE_FONT_WEIGHT(MENU_WEIGHT),
                DWRITE_FONT_STRETCH_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
            )
        } {
            Ok(font) => font,
            Err(_) => continue,
        };
        let actual_weight = unsafe { font.GetWeight() }.0;
        let face = match unsafe { font.CreateFontFace() } {
            Ok(face) => face,
            Err(_) => continue,
        };
        diag::log(&format!(
            "DirectWrite 选字：family={family} weight={actual_weight}"
        ));
        return Some(face);
    }
    None
}

/// AnalyzeScript 的数据源。菜单是单行静态文本，源只需要把它原样交出去。
#[implement(IDWriteTextAnalysisSource)]
struct TextSource {
    text: Vec<u16>,
    locale: [u16; LOCALE.len()],
}

impl IDWriteTextAnalysisSource_Impl for TextSource_Impl {
    fn GetTextAtPosition(
        &self,
        textposition: u32,
        textstring: *mut *mut u16,
        textlength: *mut u32,
    ) -> Result<()> {
        let position = textposition as usize;
        if position >= self.text.len() {
            unsafe {
                *textstring = ptr::null_mut();
                *textlength = 0;
            }
        } else {
            unsafe {
                *textstring = self.text[position..].as_ptr() as *mut u16;
                *textlength = (self.text.len() - position) as u32;
            }
        }
        Ok(())
    }

    fn GetTextBeforePosition(
        &self,
        textposition: u32,
        textstring: *mut *mut u16,
        textlength: *mut u32,
    ) -> Result<()> {
        let position = textposition as usize;
        if position == 0 || position > self.text.len() {
            unsafe {
                *textstring = ptr::null_mut();
                *textlength = 0;
            }
        } else {
            unsafe {
                *textstring = self.text.as_ptr() as *mut u16;
                *textlength = position as u32;
            }
        }
        Ok(())
    }

    fn GetParagraphReadingDirection(&self) -> DWRITE_READING_DIRECTION {
        DWRITE_READING_DIRECTION_LEFT_TO_RIGHT
    }

    fn GetLocaleName(
        &self,
        textposition: u32,
        textlength: *mut u32,
        localename: *mut *mut u16,
    ) -> Result<()> {
        let position = textposition as usize;
        if position >= self.text.len() {
            unsafe {
                *localename = ptr::null_mut();
                *textlength = 0;
            }
        } else {
            unsafe {
                *localename = self.locale.as_ptr() as *mut u16;
                *textlength = (self.text.len() - position) as u32;
            }
        }
        Ok(())
    }

    fn GetNumberSubstitution(
        &self,
        textposition: u32,
        textlength: *mut u32,
        numbersubstitution: OutRef<IDWriteNumberSubstitution>,
    ) -> Result<()> {
        let position = textposition as usize;
        unsafe {
            *textlength = self.text.len().saturating_sub(position) as u32;
        }
        numbersubstitution.write(None)?;
        Ok(())
    }
}

/// AnalyzeScript 的输出槽：本次调用是同步的，`Rc<Cell<...>>` 足够把结果带回调用方。
#[implement(IDWriteTextAnalysisSink)]
struct ScriptSink {
    script: Rc<Cell<Option<DWRITE_SCRIPT_ANALYSIS>>>,
}

impl IDWriteTextAnalysisSink_Impl for ScriptSink_Impl {
    fn SetScriptAnalysis(
        &self,
        _textposition: u32,
        _textlength: u32,
        scriptanalysis: *const DWRITE_SCRIPT_ANALYSIS,
    ) -> Result<()> {
        if !scriptanalysis.is_null() {
            self.script.set(Some(unsafe { *scriptanalysis }));
        }
        Ok(())
    }

    fn SetLineBreakpoints(
        &self,
        _textposition: u32,
        _textlength: u32,
        _linebreakpoints: *const DWRITE_LINE_BREAKPOINT,
    ) -> Result<()> {
        Ok(())
    }

    fn SetBidiLevel(
        &self,
        _textposition: u32,
        _textlength: u32,
        _explicitlevel: u8,
        _resolvedlevel: u8,
    ) -> Result<()> {
        Ok(())
    }

    fn SetNumberSubstitution(
        &self,
        _textposition: u32,
        _textlength: u32,
        _numbersubstitution: windows::core::Ref<IDWriteNumberSubstitution>,
    ) -> Result<()> {
        Ok(())
    }
}

/// 一次绘制所需的 DIB 视图。`menu::Surface` 是自顶向下的 32 位 BI_RGB。
struct DibView {
    pixels: *mut u8,
    width: i32,
    height: i32,
    stride: usize,
    top_down: bool,
}

/// 从当前 DC 选中的位图反查像素内存：`draw_text` 只拿得到 HDC，不能改签名，
/// 所以每次绘制用 `GetObjectW` 取 `DIBSECTION` 的 `bmBits`。
fn dib_view(dc: HDC) -> Option<DibView> {
    let bitmap = unsafe { GetCurrentObject(dc, OBJ_BITMAP) };
    if bitmap.is_invalid() {
        return None;
    }
    let mut info = DIBSECTION::default();
    let bytes = unsafe {
        GetObjectW(
            bitmap,
            size_of::<DIBSECTION>() as i32,
            Some(&mut info as *mut DIBSECTION as *mut c_void),
        )
    };
    if bytes == 0
        || info.dsBm.bmBits.is_null()
        || info.dsBm.bmWidth <= 0
        || info.dsBm.bmHeight == 0
        || info.dsBm.bmBitsPixel != 32
        || info.dsBm.bmWidthBytes <= 0
    {
        return None;
    }
    // `menu::Surface` 与本模块的测试面都用负 biHeight 建自顶向下 DIB；但
    // CreateDIBSection 回填到 DIBSECTION 的 `dsBmih.biHeight`/`dsBm.bmHeight` 都是正值，
    // 无法从这里判方向（已实测），所以按调用方约定固定为自顶向下。
    Some(DibView {
        pixels: info.dsBm.bmBits.cast(),
        width: info.dsBm.bmWidth,
        height: info.dsBm.bmHeight.abs(),
        stride: info.dsBm.bmWidthBytes as usize,
        top_down: true,
    })
}

impl DibView {
    fn row(&self, y: i32) -> *mut u8 {
        let row = if self.top_down {
            y
        } else {
            self.height - 1 - y
        };
        unsafe { self.pixels.add(row as usize * self.stride) }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_with(
    engine: &Engine,
    dc: HDC,
    rect: &RECT,
    text: &[u16],
    body_len: usize,
    cell_px: i32,
    background: u32,
    foreground: u32,
) -> Result<()> {
    let dib = dib_view(dc).ok_or_else(|| fail("菜单 DC 里没有可写的 32 位 DIB"))?;

    let body: Vec<u16> = text[..body_len].to_vec();
    let source: IDWriteTextAnalysisSource = TextSource {
        text: body.clone(),
        locale: LOCALE,
    }
    .into();
    let script_slot: Rc<Cell<Option<DWRITE_SCRIPT_ANALYSIS>>> = Rc::new(Cell::new(None));
    let sink: IDWriteTextAnalysisSink = ScriptSink {
        script: Rc::clone(&script_slot),
    }
    .into();
    unsafe {
        engine
            .analyzer
            .AnalyzeScript(&source, 0, body_len as u32, &sink)?;
    }
    let script = script_slot
        .get()
        .ok_or_else(|| fail("DirectWrite AnalyzeScript 没有返回脚本分析"))?;

    // 整形：先取字形，再取位置。maxGlyphCount 取文本长度的 4 倍，复杂脚本拆字也够用。
    let max_glyphs = (body_len * 4 + 16) as u32;
    let mut cluster_map = vec![0u16; body_len];
    let mut text_props = vec![DWRITE_SHAPING_TEXT_PROPERTIES::default(); body_len];
    let mut glyph_indices = vec![0u16; max_glyphs as usize];
    let mut glyph_props = vec![DWRITE_SHAPING_GLYPH_PROPERTIES::default(); max_glyphs as usize];
    let mut actual_glyphs = 0u32;
    unsafe {
        engine.analyzer.GetGlyphs(
            PCWSTR(body.as_ptr()),
            body_len as u32,
            &engine.face,
            false,
            false,
            &script,
            PCWSTR(LOCALE.as_ptr()),
            None::<&IDWriteNumberSubstitution>,
            None::<*const *const DWRITE_TYPOGRAPHIC_FEATURES>,
            None::<*const u32>,
            0,
            max_glyphs,
            cluster_map.as_mut_ptr(),
            text_props.as_mut_ptr(),
            glyph_indices.as_mut_ptr(),
            glyph_props.as_mut_ptr(),
            &mut actual_glyphs,
        )?;
    }
    if actual_glyphs == 0 {
        return Ok(());
    }
    let glyph_count = actual_glyphs as usize;

    // baseline 按 GDI `DT_VCENTER` 的语义：单元格 (ascent+descent) 在文字矩形里居中，
    // 基线 = 矩形顶 + (矩形高 - 单元格) / 2 + ascent，全部用物理像素。
    let metrics = &engine.metrics;
    let units = f32::from(metrics.designUnitsPerEm);
    let cell_units = f32::from(metrics.ascent) + f32::from(metrics.descent);
    if cell_units <= 0.0 {
        return Err(fail("DirectWrite 字体纵向度量为 0"));
    }
    // `cell_px` 是 GDI 实测的 `tmHeight`（单元格高），DirectWrite 要的是 em：
    //   em_px = cell_px × designUnitsPerEm / (ascent + descent)
    // 不能拿 `lfHeight` 当 em：Noto 的单元格是 1.448 em，直接把 px 当 em 会放大 45%，
    // 150% 缩放下菜单右边界会把最后一个字裁掉（已实测）。
    // GDI 会把单元格高的分数部分向上取整（150% 下 27.4 → 28），按 tmHeight 折算出的 em
    // 因此偏大约 2.2%：同一字串 GDI 量出 224 px、DirectWrite 排出 229 px（实测），菜单
    // 宽度又是 GDI 量出来的，行尾那个全角括号就被裁掉。按实测比例收回来，两边墨迹对齐。
    let em_px = cell_px as f32 * units / cell_units * 0.978;
    let font_size = em_px / engine.pixels_per_dip;
    let pixels_per_unit = font_size * engine.pixels_per_dip / units;
    let cell_height = cell_units * pixels_per_unit;
    let ascent_px = f32::from(metrics.ascent) * pixels_per_unit;
    let rect_height = (rect.bottom - rect.top) as f32;
    let baseline_y = (rect.top as f32 + (rect_height - cell_height) / 2.0 + ascent_px).round();
    // 横向起点保持整数像素，避免字距抖动；分数 advance 只进入 run，不做逐字取整。
    let baseline_x = rect.left as f32;

    let mut advances = vec![0f32; glyph_count];
    let mut offsets = vec![DWRITE_GLYPH_OFFSET::default(); glyph_count];
    unsafe {
        engine.analyzer.GetGlyphPlacements(
            PCWSTR(body.as_ptr()),
            cluster_map.as_ptr(),
            text_props.as_mut_ptr(),
            body_len as u32,
            glyph_indices.as_ptr(),
            glyph_props.as_ptr(),
            glyph_count as u32,
            &engine.face,
            font_size,
            false,
            false,
            &script,
            PCWSTR(LOCALE.as_ptr()),
            None::<*const *const DWRITE_TYPOGRAPHIC_FEATURES>,
            None::<*const u32>,
            0,
            advances.as_mut_ptr(),
            offsets.as_mut_ptr(),
        )?;
    }
    offsets.truncate(glyph_count);

    // GetGlyphPlacements 的 advance/offset 单位是 DIP（与传入的 fontEmSize 一致）。
    // IDWriteFactory2 的八参分析没有 pixelsPerDip，所以在这里统一换成物理像素：
    // 分数步进在换算时全程保留，只有 run 的原点/基线在下面落成整数像素。
    let to_pixels = engine.pixels_per_dip;
    for advance in &mut advances {
        *advance *= to_pixels;
    }
    for offset in &mut offsets {
        offset.advanceOffset *= to_pixels;
        offset.ascenderOffset *= to_pixels;
    }

    let mut run = DWRITE_GLYPH_RUN {
        fontFace: ManuallyDrop::new(Some(engine.face.clone())),
        fontEmSize: font_size * to_pixels,
        glyphCount: glyph_count as u32,
        glyphIndices: glyph_indices.as_ptr(),
        glyphAdvances: advances.as_ptr(),
        glyphOffsets: offsets.as_ptr(),
        isSideways: windows::core::BOOL(0),
        bidiLevel: 0,
    };

    // 八参版本的基线/字形坐标一律是物理像素；GRAYSCALE + ALIASED_1x1 才是单通道覆盖率。
    // `GetAlphaTextureBounds` 返回的是画布**绝对物理像素**矩形（已实测包含传入的基线基点），
    // 落位时直接使用 bounds，不能再叠加 origin。
    let analysis_result = unsafe {
        engine.factory2.CreateGlyphRunAnalysis(
            &run,
            None,
            DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
            DWRITE_MEASURING_MODE_NATURAL,
            DWRITE_GRID_FIT_MODE_ENABLED,
            DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE,
            baseline_x.round(),
            baseline_y,
        )
    };
    unsafe {
        ManuallyDrop::drop(&mut run.fontFace);
    }
    let analysis = analysis_result?;

    let bounds = unsafe { analysis.GetAlphaTextureBounds(DWRITE_TEXTURE_ALIASED_1x1)? };
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    if width <= 0 || height <= 0 {
        return Ok(());
    }
    let mut coverage = vec![0u8; (width * height) as usize];
    unsafe {
        analysis.CreateAlphaTexture(DWRITE_TEXTURE_ALIASED_1x1, &bounds, &mut coverage)?;
    }

    let origin_x = bounds.left;
    let origin_y = bounds.top;
    let bg = bgr(background);
    let fg = bgr(foreground);
    let clip_left = rect.left.max(0);
    let clip_right = rect.right.min(dib.width);
    let clip_top = rect.top.max(0);
    let clip_bottom = rect.bottom.min(dib.height);

    for ty in 0..height {
        let y = origin_y + ty;
        if y < clip_top || y >= clip_bottom {
            continue;
        }
        let row = dib.row(y);
        for tx in 0..width {
            let x = origin_x + tx;
            if x < clip_left || x >= clip_right {
                continue;
            }
            let alpha = coverage[(ty * width + tx) as usize];
            if alpha == 0 {
                continue;
            }
            let index = (x as usize) * 4;
            let base = unsafe { row.add(index) };
            let cover = u32::from(alpha);
            for channel in 0..3 {
                let low = u32::from(bg[channel]);
                let high = u32::from(fg[channel]);
                let value = (low * (255 - cover) + high * cover + 127) / 255;
                unsafe {
                    *base.add(channel) = value as u8;
                }
            }
        }
    }
    Ok(())
}

/// COLORREF (`0x00BBGGRR`) → 字节序 `[b, g, r]`，与 DIB 内存顺序一致。
fn bgr(color: u32) -> [u8; 3] {
    [
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::zeroed;

    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, DrawTextW, GetTextMetricsW,
        SelectObject, SetBkColor, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, HBITMAP, HBRUSH, HFONT, HGDIOBJ,
        OPAQUE, TEXTMETRICW,
    };
    use windows::Win32::Graphics::Gdi::{CreateSolidBrush, FillRect, HDC};

    use crate::tray::{mark_geometry, menu_font, menu_palette, MENU_GAP, MENU_PAD_L, MENU_PAD_R};

    /// 测试专用：自顶向下 32 位 DIB + 内存 DC，和 `menu::Surface` 同样的形态。
    struct TestSurface {
        dc: HDC,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
        pixels: *mut u8,
        width: i32,
        height: i32,
    }

    impl TestSurface {
        fn new(width: i32, height: i32) -> TestSurface {
            unsafe {
                let dc = CreateCompatibleDC(None);
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: width,
                        biHeight: -height,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..zeroed()
                    },
                    ..zeroed()
                };
                let mut bits: *mut c_void = ptr::null_mut();
                let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)
                    .expect("测试 DIB");
                assert!(!bits.is_null());
                let previous = SelectObject(dc, HGDIOBJ(bitmap.0));
                Self {
                    dc,
                    bitmap,
                    previous,
                    pixels: bits.cast(),
                    width,
                    height,
                }
            }
        }

        fn fill(&self, color: u32) {
            let brush: HBRUSH = unsafe { CreateSolidBrush(COLORREF(color)) };
            let rect = RECT {
                left: 0,
                top: 0,
                right: self.width,
                bottom: self.height,
            };
            unsafe {
                FillRect(self.dc, &rect, brush);
                let _ = DeleteObject(HGDIOBJ(brush.0));
            }
        }

        fn rgb(&self, x: i32, y: i32) -> [u8; 3] {
            let index = ((y * self.width + x) * 4) as usize;
            unsafe {
                [
                    *self.pixels.add(index),
                    *self.pixels.add(index + 1),
                    *self.pixels.add(index + 2),
                ]
            }
        }

        fn stats(&self, background: u32) -> Stats {
            let bg = bgr(background);
            let mut stats = Stats::default();
            for y in 0..self.height {
                for x in 0..self.width {
                    let px = self.rgb(x, y);
                    if px != bg {
                        stats.ink += 1;
                        stats.left = stats.left.min(x);
                        stats.top = stats.top.min(y);
                        stats.right = stats.right.max(x);
                        stats.bottom = stats.bottom.max(y);
                    }
                    if px.iter().max().unwrap() - px.iter().min().unwrap() > 0 {
                        stats.colored += 1;
                    }
                }
            }
            stats
        }
    }

    impl Drop for TestSurface {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.dc, self.previous);
                let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
                let _ = DeleteDC(self.dc);
            }
        }
    }

    #[derive(Debug)]
    struct Stats {
        ink: u32,
        colored: u32,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    impl Default for Stats {
        fn default() -> Self {
            Self {
                ink: 0,
                colored: 0,
                left: i32::MAX,
                top: i32::MAX,
                right: i32::MIN,
                bottom: i32::MIN,
            }
        }
    }

    fn gdi_draw(
        surface: &TestSurface,
        rect: &RECT,
        text: &mut [u16],
        font: HFONT,
        background: u32,
        foreground: u32,
    ) {
        unsafe {
            let previous = SelectObject(surface.dc, HGDIOBJ(font.0));
            SetBkMode(surface.dc, OPAQUE);
            SetBkColor(surface.dc, COLORREF(background));
            SetTextColor(surface.dc, COLORREF(foreground));
            let mut draw = *rect;
            DrawTextW(
                surface.dc,
                text,
                &mut draw,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
            SelectObject(surface.dc, previous);
        }
    }

    #[test]
    #[ignore = "度量探针：cargo test --release --bin litesound -- --ignored dw_metrics_probe --nocapture"]
    fn dw_metrics_probe() {
        // 真机菜单进程是 PerMonitorV2，测试进程默认 96 DPI；先切到同一感知级别，
        // 否则 ppd/px 都是 96 DPI 的值，量出来的比例在 150% 缩放上不成立。
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        let (font, px) = menu_font();
        let surface = TestSurface::new(64, 64);
        let palette = menu_palette(true);
        surface.fill(palette.bg);
        let mut tm = TEXTMETRICW::default();
        unsafe {
            let previous = SelectObject(surface.dc, HGDIOBJ(font.0));
            let _ = GetTextMetricsW(surface.dc, &mut tm);
            SelectObject(surface.dc, previous);
        }
        // 先强制初始化一次引擎，下面才能读到字体度量。
        let probe: Vec<u16> = "A".encode_utf16().collect();
        let _ = draw(
            surface.dc,
            &RECT {
                left: 4,
                top: 4,
                right: 60,
                bottom: 60,
            },
            &probe,
            px,
            tm.tmHeight,
            palette.bg,
            palette.dim,
        );
        println!(
            "GDI px={px} tmHeight={} tmAscent={} tmDescent={} tmExternalLeading={} tmWeight={}",
            tm.tmHeight, tm.tmAscent, tm.tmDescent, tm.tmExternalLeading, tm.tmWeight
        );
        let engine = ENGINE
            .with(|cell| cell.get().cloned().flatten())
            .expect("引擎");
        let metrics = &engine.metrics;
        println!(
            "DW upem={} ascent={} descent={} lineGap={} ppd={}",
            metrics.designUnitsPerEm,
            metrics.ascent,
            metrics.descent,
            metrics.lineGap,
            engine.pixels_per_dip
        );
    }

    #[test]
    #[ignore = "release 下跑：cargo test --release --bin litesound -- --ignored dwrite_gray_vs_gdi --nocapture"]
    fn dwrite_gray_vs_gdi() {
        let sample = "扬声器 (网易虚拟音频设备)";
        let mut text: Vec<u16> = sample.encode_utf16().chain(std::iter::once(0)).collect();
        let (font, px) = menu_font();
        let palette = menu_palette(true);
        let (marker_width, _) = mark_geometry().expect("标记槽");

        let width = 560;
        let height = 70;
        let rect = RECT {
            left: MENU_PAD_L + marker_width + MENU_GAP,
            top: 3,
            right: width - MENU_PAD_R,
            bottom: 58,
        };

        let gdi = TestSurface::new(width, height);
        gdi.fill(palette.bg);
        gdi_draw(&gdi, &rect, &mut text, font, palette.bg, palette.dim);
        let gdi_stats = gdi.stats(palette.bg);

        let dw = TestSurface::new(width, height);
        dw.fill(palette.bg);
        let mut tm = TEXTMETRICW::default();
        unsafe {
            let previous = SelectObject(dw.dc, HGDIOBJ(font.0));
            let _ = GetTextMetricsW(dw.dc, &mut tm);
            SelectObject(dw.dc, previous);
        }
        let ok = draw(
            dw.dc,
            &rect,
            &text,
            px,
            tm.tmHeight,
            palette.bg,
            palette.dim,
        );
        let dw_stats = dw.stats(palette.bg);

        println!(
            "DirectWrite 覆盖率路径：墨迹 {} px，彩边 {} px，bbox=({}, {}, {}, {})，ok={ok}",
            dw_stats.ink,
            dw_stats.colored,
            dw_stats.left,
            dw_stats.top,
            dw_stats.right,
            dw_stats.bottom
        );
        println!(
            "GDI DrawTextW 路径：墨迹 {} px，彩边 {} px，bbox=({}, {}, {}, {})",
            gdi_stats.ink,
            gdi_stats.colored,
            gdi_stats.left,
            gdi_stats.top,
            gdi_stats.right,
            gdi_stats.bottom
        );
        assert!(ok, "DirectWrite 路径应可用");
        assert_eq!(dw_stats.colored, 0, "DirectWrite 输出必须是纯灰度");
    }
}
