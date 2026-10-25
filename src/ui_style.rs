//! 全局视觉规范：颜色、字号、控件尺寸与按钮样式。
//!
//! 设计令牌允许暂时保留未使用档位，后续界面直接复用，避免再次散落硬编码。
#![allow(dead_code, reason = "统一设计令牌，部分档位预留给后续界面")]
//!
//! 所有界面模块只允许从这里取样式，不要在绘制代码里散落新的颜色、字号或按钮尺寸。
//! 具体规范说明见 `docs/ui-style-guide.md`。

use egui::{Color32, Response, RichText, Ui};

/// Sub2API 双主题调色板。
///
/// 颜色取自 Sub2API 前端 Tailwind 配置：
/// 主色 primary-500 `#14b8a6`；浅色主题使用 gray-50 / white；
/// 深色主题使用 dark-950 / dark-800 / dark-700。
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// 面板背景。
    pub(crate) panel_bg: egui::Color32,
    /// 面板和卡片边框。
    pub(crate) panel_border: egui::Color32,
    /// 普通卡片背景。
    pub(crate) card_bg: egui::Color32,
    /// 卡片悬浮背景。
    pub(crate) hover_card_bg: egui::Color32,
    /// 空状态/输入控件背景。
    pub(crate) empty_bg: egui::Color32,
    /// 主要文字。
    pub(crate) text_primary: egui::Color32,
    /// 次要文字。
    pub(crate) text_secondary: egui::Color32,
    /// 弱化文字。
    pub(crate) text_dim: egui::Color32,
    /// 悬浮时提亮/加深的文字。
    pub(crate) text_hover: egui::Color32,
    /// 品牌强调色。
    pub(crate) accent: egui::Color32,
    /// 强调色上的文字。
    pub(crate) on_accent: egui::Color32,
    /// 图表面积填充。
    pub(crate) chart_fill: egui::Color32,
    /// 错误主色。
    pub(crate) danger: egui::Color32,
    /// 错误提示标题。
    pub(crate) danger_text: egui::Color32,
    /// 错误提示正文。
    pub(crate) danger_text_soft: egui::Color32,
    /// 错误提示背景。
    pub(crate) danger_bg: egui::Color32,
    /// 错误提示边框。
    pub(crate) danger_border: egui::Color32,
    /// 危险操作按钮底色。
    pub(crate) danger_button: egui::Color32,
    /// 危险操作按钮文字色。
    pub(crate) danger_button_text: egui::Color32,
    /// 刷新中/信息色。
    pub(crate) info: egui::Color32,
    /// 离线状态色。
    pub(crate) offline: egui::Color32,
    /// 网格线/分隔细线。
    pub(crate) grid_line: egui::Color32,
    /// 极淡的悬浮背景。
    pub(crate) overlay_subtle: egui::Color32,
    /// 缩放角标。
    pub(crate) resize_handle: egui::Color32,
    /// 图表悬浮提示背景。
    pub(crate) tooltip_bg: egui::Color32,
    /// 输入框背景。
    pub(crate) input_bg: egui::Color32,
}

/// 夜间主题：Sub2API dark-950 + dark-800 + primary-400。
const DARK_PALETTE: Palette = Palette {
    panel_bg: egui::Color32::from_rgb(2, 6, 23),
    panel_border: egui::Color32::from_rgb(51, 65, 85),
    card_bg: egui::Color32::from_rgb(30, 41, 59),
    hover_card_bg: egui::Color32::from_rgb(51, 65, 85),
    empty_bg: egui::Color32::from_rgb(15, 23, 42),
    text_primary: egui::Color32::from_rgb(243, 244, 246),
    text_secondary: egui::Color32::from_rgb(203, 213, 225),
    text_dim: egui::Color32::from_rgb(148, 163, 184),
    text_hover: egui::Color32::WHITE,
    accent: egui::Color32::from_rgb(45, 212, 191),
    on_accent: egui::Color32::from_rgb(4, 47, 46),
    chart_fill: egui::Color32::from_rgba_unmultiplied_const(45, 212, 191, 36),
    danger: egui::Color32::from_rgb(248, 113, 113),
    danger_text: egui::Color32::from_rgb(252, 165, 165),
    danger_text_soft: egui::Color32::from_rgb(254, 202, 202),
    danger_bg: egui::Color32::from_rgb(69, 26, 26),
    danger_border: egui::Color32::from_rgb(153, 27, 27),
    danger_button: egui::Color32::from_rgb(127, 29, 29),
    danger_button_text: egui::Color32::from_rgb(254, 202, 202),
    info: egui::Color32::from_rgb(96, 165, 250),
    offline: egui::Color32::from_rgb(148, 163, 184),
    grid_line: egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 12),
    overlay_subtle: egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 8),
    resize_handle: egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 70),
    tooltip_bg: egui::Color32::from_rgba_unmultiplied_const(15, 23, 42, 240),
    input_bg: egui::Color32::from_rgb(15, 23, 42),
};

/// 日间主题：Sub2API gray-50 + white + primary-600。
const LIGHT_PALETTE: Palette = Palette {
    panel_bg: egui::Color32::from_rgb(248, 250, 252),
    panel_border: egui::Color32::from_rgb(226, 232, 240),
    card_bg: egui::Color32::WHITE,
    hover_card_bg: egui::Color32::from_rgb(241, 245, 249),
    empty_bg: egui::Color32::from_rgb(241, 245, 249),
    text_primary: egui::Color32::from_rgb(15, 23, 42),
    text_secondary: egui::Color32::from_rgb(71, 85, 105),
    text_dim: egui::Color32::from_rgb(148, 163, 184),
    text_hover: egui::Color32::from_rgb(2, 6, 23),
    accent: egui::Color32::from_rgb(13, 148, 136),
    on_accent: egui::Color32::WHITE,
    chart_fill: egui::Color32::from_rgba_unmultiplied_const(13, 148, 136, 28),
    danger: egui::Color32::from_rgb(220, 38, 38),
    danger_text: egui::Color32::from_rgb(185, 28, 28),
    danger_text_soft: egui::Color32::from_rgb(153, 27, 27),
    danger_bg: egui::Color32::from_rgb(254, 242, 242),
    danger_border: egui::Color32::from_rgb(254, 202, 202),
    danger_button: egui::Color32::from_rgb(252, 165, 165),
    danger_button_text: egui::Color32::from_rgb(127, 29, 29),
    info: egui::Color32::from_rgb(37, 99, 235),
    offline: egui::Color32::from_rgb(100, 116, 139),
    grid_line: egui::Color32::from_rgba_unmultiplied_const(0, 0, 0, 12),
    overlay_subtle: egui::Color32::from_rgba_unmultiplied_const(0, 0, 0, 8),
    resize_handle: egui::Color32::from_rgba_unmultiplied_const(0, 0, 0, 70),
    tooltip_bg: egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 245),
    input_bg: egui::Color32::WHITE,
};

thread_local! {
    /// 当前 UI 线程使用的调色板。托盘只有一个界面线程，使用线程局部状态
    /// 可以避免把 Palette 参数穿过几十个纯绘制函数。
    static PALETTE: std::cell::Cell<Palette> = const { std::cell::Cell::new(DARK_PALETTE) };
}

/// 当前主题调色板。
pub(crate) fn theme() -> Palette {
    PALETTE.with(std::cell::Cell::get)
}

/// 根据夜间/日间选择返回调色板。
pub(crate) fn palette_for(dark: bool) -> Palette {
    if dark { DARK_PALETTE } else { LIGHT_PALETTE }
}

/// 设置当前线程的调色板。
pub(crate) fn set_palette(dark: bool) {
    PALETTE.with(|palette| palette.set(palette_for(dark)));
}

/// 把主题同步到 egui 的默认控件样式（输入框、ComboBox 弹层、按钮等）。
pub(crate) fn apply_egui_visuals(ctx: &egui::Context, dark: bool) {
    let palette = palette_for(dark);
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.dark_mode = dark;
    visuals.panel_fill = palette.panel_bg;
    visuals.window_fill = palette.panel_bg;
    visuals.extreme_bg_color = palette.input_bg;
    visuals.faint_bg_color = palette.empty_bg;
    visuals.override_text_color = Some(palette.text_primary);
    visuals.hyperlink_color = palette.accent;
    visuals.error_fg_color = palette.danger;
    visuals.warn_fg_color = egui::Color32::from_rgb(245, 158, 11);
    visuals.window_stroke = egui::Stroke::new(1.0, palette.panel_border);
    visuals.selection.bg_fill = palette.chart_fill;
    visuals.selection.stroke = egui::Stroke::new(1.0, palette.accent);

    visuals.widgets.noninteractive.bg_fill = palette.card_bg;
    visuals.widgets.noninteractive.weak_bg_fill = palette.card_bg;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, palette.panel_border);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, palette.text_secondary);

    visuals.widgets.inactive.bg_fill = palette.empty_bg;
    visuals.widgets.inactive.weak_bg_fill = palette.empty_bg;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, palette.panel_border);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, palette.text_primary);

    visuals.widgets.hovered.bg_fill = palette.hover_card_bg;
    visuals.widgets.hovered.weak_bg_fill = palette.hover_card_bg;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, palette.accent);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, palette.text_hover);

    visuals.widgets.active.bg_fill = palette.hover_card_bg;
    visuals.widgets.active.weak_bg_fill = palette.hover_card_bg;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, palette.accent);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, palette.text_hover);

    visuals.widgets.open = visuals.widgets.active;
    ctx.all_styles_mut(|style| {
        style.visuals = visuals.clone();
        // 托盘是操作面板而不是阅读器：禁止鼠标拖选 Label 文本，
        // 避免拖动标题/滑动用量区时意外出现蓝底选中文字。
        style.interaction.selectable_labels = false;
        style.interaction.multi_widget_text_select = false;
    });
}

/// 线性插值两个颜色（用于悬浮淡入淡出）。
pub(crate) fn lerp_color(from: egui::Color32, to: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    egui::Color32::from_rgba_premultiplied(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
        mix(from.a(), to.a()),
    )
}

/// 全局字号增量：需求方反馈整体字体偏小，所有 .size(x) 统一加 1 码。
pub(crate) const FONT_BOOST: f32 = 1.0;

/// 把设计稿字号放大 [FONT_BOOST] 后返回，便于统一调大/回调。
pub(crate) const fn fs(base: f32) -> f32 {
    base + FONT_BOOST
}

/// 用量区专用缩放：在全局字号基础上再放大一档。
pub(crate) const fn usage_scale(base: f32) -> f32 {
    fs(base) + 1.0
}

/// 语义化字号。数值已经包含全局字号增量，绘制代码不要再写 `size(10.0)`。
pub mod font {
    use super::fs;

    pub const CAPTION: f32 = fs(8.5);
    pub const SMALL: f32 = fs(9.0);
    pub const SMALL_PLUS: f32 = fs(9.5);
    pub const BODY_SMALL: f32 = fs(10.0);
    pub const BODY_SMALL_PLUS: f32 = fs(10.5);
    pub const BODY: f32 = fs(11.0);
    pub const BODY_LARGE: f32 = fs(12.0);
    pub const SUBHEADING: f32 = fs(13.0);
    pub const HEADING: f32 = fs(14.0);
    pub const TITLE: f32 = fs(15.0);
}

/// 用量区专用字号（在全局字号基础上再放大一档）。
pub mod usage_font {
    use super::fs;

    pub const MICRO: f32 = fs(7.5) + 1.0;
    pub const TINY: f32 = fs(8.0) + 1.0;
    pub const CAPTION: f32 = fs(8.5) + 1.0;
    pub const SMALL: f32 = fs(9.0) + 1.0;
    pub const BODY_SMALL: f32 = fs(10.0) + 1.0;
    pub const BODY: f32 = fs(11.0) + 1.0;
}

/// 语义化控件尺寸与间距。
pub mod size {
    pub const CONTROL_HEIGHT: f32 = 26.0;
    pub const INPUT_HEIGHT: f32 = 28.0;
    pub const INPUT_PAD_X: i8 = 8;
    pub const INPUT_PAD_Y: i8 = 6;
    pub const DATE_PICKER_WIDTH: f32 = 108.0;
    pub const TIME_PICKER_WIDTH: f32 = 56.0;
    pub const CURRENCY_PREFIX_WIDTH: f32 = 16.0;
    pub const NUMBER_INPUT_WIDTH: f32 = 80.0;
    pub const TOOLTIP_MAX_WIDTH: f32 = 240.0;
    pub const FLOATING_BALL_SIZE: f32 = 64.0;
    pub const FLOATING_BALL_COLLAPSED_WIDTH: f32 = 18.0;
    pub const FLOATING_BALL_MARGIN: f32 = 8.0;
    pub const FLOATING_BALL_EDGE_THRESHOLD: f32 = 16.0;
    pub const FLOATING_BALL_PICKER_WIDTH: f32 = 150.0;
    pub const SETTINGS_SCROLL_RESERVED: f32 = 210.0;
    pub const SETTINGS_SCROLL_MIN: f32 = 220.0;
    pub const BUTTON_XS_W: f32 = 76.0;
    pub const BUTTON_XS_H: f32 = 22.0;
    pub const BUTTON_TINY_W: f32 = 64.0;
    pub const BUTTON_TINY_H: f32 = 22.0;
    pub const BUTTON_KEY_W: f32 = 64.0;
    pub const BUTTON_KEY_H: f32 = 26.0;
    pub const BUTTON_HEADER_W: f32 = 104.0;
    pub const BUTTON_HEADER_H: f32 = 24.0;
    pub const BUTTON_FORM_W: f32 = 80.0;
    pub const BUTTON_FORM_H: f32 = 26.0;
    pub const BUTTON_FORM_MD_W: f32 = 100.0;
    pub const BUTTON_FORM_MD_H: f32 = 26.0;
    pub const BUTTON_WIDE_W: f32 = 120.0;
    pub const BUTTON_WIDE_H: f32 = 22.0;
    pub const BUTTON_SM_W: f32 = 88.0;
    pub const BUTTON_SM_H: f32 = 24.0;
    pub const BUTTON_MD_W: f32 = 96.0;
    pub const BUTTON_MD_H: f32 = 26.0;
    pub const BUTTON_LG_W: f32 = 108.0;
    pub const BUTTON_LG_H: f32 = 28.0;
    pub const MODAL_WIDTH: f32 = 300.0;
    pub const SETTINGS_WIDTH: f32 = 280.0;
    pub const RADIUS_HAIRLINE: u8 = 1;
    pub const RADIUS_XS: u8 = 5;
    pub const RADIUS_SM: u8 = 4;
    pub const RADIUS_MD: u8 = 6;
    pub const RADIUS_LG: u8 = 10;
    pub const RADIUS_XL: u8 = 12;
    pub const CARD_RADIUS: u8 = RADIUS_LG;
    pub const MODAL_RADIUS: u8 = RADIUS_XL;
    pub const MARGIN_SM: i8 = 10;
    pub const MARGIN_MD: i8 = 12;
    pub const MARGIN_LG: i8 = 16;
    pub const MODAL_MARGIN: i8 = MARGIN_LG;
    pub const CARD_PADDING: f32 = 10.0;
    pub const SECTION_GAP: f32 = 8.0;
    pub const FORM_GAP: f32 = 8.0;
    pub const ICON_MD: f32 = 16.0;
    pub const HEADER_ROW_HEIGHT: f32 = 20.0;
    pub const HEADER_CONTROLS_WIDTH: f32 = 120.0;
    pub const DRAG_HANDLE_WIDTH: f32 = 12.0;
    pub const DRAG_HANDLE_HEIGHT: f32 = 18.0;
    pub const USAGE_SELECT_WIDTH: f32 = 108.0;
    pub const USAGE_AXIS_HEIGHT: f32 = 14.0;
    pub const USAGE_PLOT_HEIGHT: f32 = 84.0;
    pub const TOAST_ALPHA: u8 = 220;
    pub const MODAL_MAX_WIDTH: f32 = 320.0;
    pub const FORM_INPUT_WIDTH: f32 = 220.0;
    pub const ACCENT_BAR_WIDTH: f32 = 3.0;
    pub const ACCENT_BAR_HEIGHT: f32 = 32.0;
}

/// 语义化间距。
pub mod space {
    pub const HAIRLINE: f32 = 2.0;
    pub const XXS: f32 = 3.0;
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 6.0;
    pub const MD: f32 = 8.0;
    pub const LG: f32 = 10.0;
    pub const XL: f32 = 12.0;
    pub const XXL: f32 = 14.0;
    pub const XXXL: f32 = 16.0;
}

/// 按钮语义色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonTone {
    /// 普通次要按钮。
    Neutral,
    /// 品牌色主按钮。
    Primary,
    /// 危险操作按钮。
    Danger,
    /// 透明背景的轻量按钮。
    Ghost,
}

impl ButtonTone {
    /// 文字颜色。
    pub fn text_color(self) -> Color32 {
        match self {
            Self::Neutral | Self::Ghost => theme().text_primary,
            Self::Primary => theme().on_accent,
            Self::Danger => theme().danger_button_text,
        }
    }

    /// 按钮背景色。
    pub fn fill(self) -> Option<Color32> {
        match self {
            Self::Neutral | Self::Ghost => None,
            Self::Primary => Some(theme().accent),
            Self::Danger => Some(theme().danger_button),
        }
    }
}

/// 按钮尺寸档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonSize {
    /// 76×22，总览卡片上的小按钮。
    Xs,
    /// 64×22，登录等紧凑按钮。
    Tiny,
    /// 64×26，Key 悬浮操作按钮。
    KeyAction,
    /// 104×24，账号标题行操作按钮。
    Header,
    /// 80×26，表单底部按钮。
    Form,
    /// 100×26。
    FormMd,
    /// 120×22，链接类宽按钮。
    Wide,
    /// 88×24。
    Sm,
    /// 96×26。
    Md,
    /// 108×28。
    Lg,
    /// 只设置高度，宽度随内容。
    Auto,
}

impl ButtonSize {
    fn dimensions(self) -> (f32, f32, f32) {
        match self {
            Self::Xs => (size::BUTTON_XS_W, size::BUTTON_XS_H, font::BODY_SMALL),
            Self::Tiny => (size::BUTTON_TINY_W, size::BUTTON_TINY_H, font::BODY_SMALL),
            Self::KeyAction => (
                size::BUTTON_KEY_W,
                size::BUTTON_KEY_H,
                font::BODY_SMALL_PLUS,
            ),
            Self::Header => (
                size::BUTTON_HEADER_W,
                size::BUTTON_HEADER_H,
                font::BODY_SMALL_PLUS,
            ),
            Self::Form => (size::BUTTON_FORM_W, size::BUTTON_FORM_H, font::BODY_LARGE),
            Self::FormMd => (
                size::BUTTON_FORM_MD_W,
                size::BUTTON_FORM_MD_H,
                font::BODY_SMALL_PLUS,
            ),
            Self::Wide => (size::BUTTON_WIDE_W, size::BUTTON_WIDE_H, font::BODY_SMALL),
            Self::Sm => (size::BUTTON_SM_W, size::BUTTON_SM_H, font::BODY_SMALL),
            Self::Md => (size::BUTTON_MD_W, size::BUTTON_MD_H, font::BODY_SMALL_PLUS),
            Self::Lg => (size::BUTTON_LG_W, size::BUTTON_LG_H, font::BODY),
            Self::Auto => (0.0, size::BUTTON_SM_H, font::BODY_SMALL),
        }
    }
}

/// 创建一个统一风格按钮。
pub fn button(
    text: impl Into<String>,
    tone: ButtonTone,
    size: ButtonSize,
) -> egui::Button<'static> {
    let (_, _, font_size) = size.dimensions();
    let mut rich = RichText::new(text.into())
        .size(font_size)
        .color(tone.text_color());
    if matches!(tone, ButtonTone::Primary) {
        rich = rich.strong();
    }
    rich_button(rich, tone, size)
}

/// 用调用方提供的 RichText 创建统一按钮。
///
/// 用于选中态切换按钮：文字颜色由调用方按选中态设置，但尺寸、填充和按钮语义
/// 仍走统一规范。
pub fn rich_button(text: RichText, tone: ButtonTone, size: ButtonSize) -> egui::Button<'static> {
    let (width, height, _) = size.dimensions();
    let mut button = egui::Button::new(text).min_size(egui::vec2(width, height));
    if let Some(fill) = tone.fill() {
        button = button.fill(fill);
    }
    button
}

/// 在给定 Ui 上添加统一风格按钮。
pub fn add_button(
    ui: &mut Ui,
    text: impl Into<String>,
    tone: ButtonTone,
    size: ButtonSize,
) -> Response {
    let (width, height, _) = size.dimensions();
    let button = button(text, tone, size);
    ui.allocate_ui_with_layout(
        egui::vec2(width.max(1.0), height),
        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
        |ui| ui.add(button),
    )
    .inner
}

/// 在给定 Ui 上添加可启用/禁用的统一风格按钮。
pub fn add_enabled_button(
    ui: &mut Ui,
    enabled: bool,
    text: impl Into<String>,
    tone: ButtonTone,
    size: ButtonSize,
) -> Response {
    let (width, height, _) = size.dimensions();
    let button = button(text, tone, size);
    ui.allocate_ui_with_layout(
        egui::vec2(width.max(1.0), height),
        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
        |ui| ui.add_enabled(enabled, button),
    )
    .inner
}

/// 模态遮罩颜色。
pub fn modal_backdrop() -> Color32 {
    Color32::from_black_alpha(150)
}

/// 表单分组卡片 Frame。
pub fn section_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme().empty_bg)
        .corner_radius(egui::CornerRadius::same(size::RADIUS_LG))
        .stroke(egui::Stroke::new(1.0, theme().panel_border))
        .inner_margin(egui::Margin::same(size::MARGIN_MD))
}

/// 错误提示条。
pub fn error_banner(ui: &mut Ui, text: &str) {
    egui::Frame::new()
        .fill(theme().danger_bg)
        .corner_radius(egui::CornerRadius::same(size::RADIUS_LG))
        .stroke(egui::Stroke::new(1.0, theme().danger_border))
        .inner_margin(egui::Margin::same(size::MARGIN_SM))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(text)
                    .size(font::SMALL_PLUS)
                    .color(theme().danger_text),
            );
        });
}

/// 分组/卡片标题。
pub fn section_title(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .size(font::SUBHEADING)
            .strong()
            .color(theme().text_primary),
    );
}

/// 统一样式的数字输入框。
pub fn drag_number<T: egui::emath::Numeric>(
    ui: &mut Ui,
    value: &mut T,
    range: std::ops::RangeInclusive<T>,
    suffix: &str,
    speed: f64,
) -> Response {
    ui.add_sized(
        [size::NUMBER_INPUT_WIDTH, size::INPUT_HEIGHT],
        egui::DragValue::new(value)
            .range(range)
            .suffix(suffix)
            .speed(speed),
    )
}

/// 表单字段标签。
pub fn field_label(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .size(font::BODY_SMALL)
            .color(theme().text_secondary),
    );
}

/// 表单说明文字。
pub fn hint_label(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .size(font::SMALL)
            .color(theme().text_dim),
    );
}

/// 统一样式的单行输入框。
pub fn singleline_input(ui: &mut Ui, text: &mut String, hint: &str) -> Response {
    ui.add_sized(
        [ui.available_width(), size::INPUT_HEIGHT],
        egui::TextEdit::singleline(text)
            .hint_text(hint)
            .desired_width(f32::INFINITY)
            .margin(egui::Margin::symmetric(
                size::INPUT_PAD_X,
                size::INPUT_PAD_Y,
            )),
    )
}

/// 统一样式的密码输入框。
pub fn password_input(ui: &mut Ui, text: &mut String, hint: &str) -> Response {
    ui.add_sized(
        [ui.available_width(), size::INPUT_HEIGHT],
        egui::TextEdit::singleline(text)
            .hint_text(hint)
            .password(true)
            .desired_width(f32::INFINITY)
            .margin(egui::Margin::symmetric(
                size::INPUT_PAD_X,
                size::INPUT_PAD_Y,
            )),
    )
}

/// 统一样式的日期选择按钮。
pub fn date_picker(ui: &mut Ui, date: &mut jiff::civil::Date, id_salt: &str) -> Response {
    ui.add_sized(
        [size::DATE_PICKER_WIDTH, size::INPUT_HEIGHT],
        egui_extras::DatePickerButton::new(date)
            .id_salt(id_salt)
            .show_icon(false)
            .calendar(false)
            .arrows(false)
            .combo_boxes(true)
            .format("%Y-%m-%d"),
    )
}

/// 统一样式的时间/数字输入。
pub fn number_input(
    ui: &mut Ui,
    value: &mut i8,
    range: std::ops::RangeInclusive<i8>,
    suffix: &str,
) -> Response {
    ui.add_sized(
        [size::TIME_PICKER_WIDTH, size::INPUT_HEIGHT],
        egui::DragValue::new(value).range(range).suffix(suffix),
    )
}

/// 统一样式的货币输入。
pub fn currency_input(ui: &mut Ui, text: &mut String, prefix: &str, hint: &str) -> Response {
    ui.horizontal(|ui| {
        ui.set_height(size::INPUT_HEIGHT);
        ui.add_sized(
            [size::CURRENCY_PREFIX_WIDTH, size::INPUT_HEIGHT],
            egui::Label::new(RichText::new(prefix).size(font::BODY).color(theme().accent)),
        );
        ui.add_sized(
            [ui.available_width(), size::INPUT_HEIGHT],
            egui::TextEdit::singleline(text)
                .hint_text(hint)
                .desired_width(f32::INFINITY)
                .margin(egui::Margin::symmetric(
                    size::INPUT_PAD_X,
                    size::INPUT_PAD_Y,
                )),
        )
    })
    .inner
}

/// 底部提示浮层的半透明背景。
pub fn toast_background() -> Color32 {
    let card = theme().card_bg;
    Color32::from_rgba_unmultiplied(card.r(), card.g(), card.b(), size::TOAST_ALPHA)
}

/// 统一卡片 Frame。
pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme().card_bg)
        .corner_radius(egui::CornerRadius::same(size::CARD_RADIUS))
        .stroke(egui::Stroke::new(1.0, theme().panel_border))
}

/// 统一弹窗 Frame。
pub fn modal_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme().panel_bg)
        .corner_radius(egui::CornerRadius::same(size::MODAL_RADIUS))
        .stroke(egui::Stroke::new(1.0, theme().panel_border))
        .inner_margin(egui::Margin::same(size::MODAL_MARGIN))
}
