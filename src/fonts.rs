//! 中文字体加载。
//!
//! egui 内置的默认字体只覆盖拉丁字符，中文会渲染成“豆腐块”（□□□）。
//! 托盘面板里的文案全是中文，因此必须在启动时补一份中文字体。
//!
//! 这里选择**运行时从系统目录加载**，而不是把字体文件塞进仓库：
//!
//! * 字体版权干净：微软雅黑、苹方等都不允许随程序分发；
//! * 二进制体积小：完整中文字库动辄 10MB 以上；
//! * 显示效果跟随系统，用户换字体后界面观感一致。
//!
//! 代价是：如果系统里这几个候选字体都被删掉了，中文就退回默认字体（显示为方块），
//! 所以这里会打印一条警告日志，方便排查。

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, FontId, TextStyle};

/// 候选字体列表：`(字体文件路径, 文件内的字形索引)`。
///
/// 顺序即优先级，越靠前越先尝试：
///
/// | 平台 | 字体 | 说明 |
/// | --- | --- | --- |
/// | Windows | 微软雅黑 `msyh.ttc` | Win7 之后默认自带，字形最完整 |
/// | Windows | 等线 `Deng.ttf` | 精简版 Windows 也可能存在 |
/// | Windows | 黑体 `simhei.ttf` | 老系统的兜底方案 |
/// | macOS | 苹方 `PingFang.ttc` | 系统默认中文字体 |
/// | macOS | 华文黑体 `STHeiti Light.ttc` | 旧版 macOS 兜底 |
/// | macOS | 宋体-简 `Songti.ttc` | Supplemental 目录里的兜底 |
///
/// 注意 `.ttc` 是“字体集合”，一个文件里打包了多个 face；
/// 这里统一取第 0 个 face，即常规字重。
const CANDIDATES: &[(&str, u32)] = &[
    (r"C:\Windows\Fonts\msyh.ttc", 0),
    (r"C:\Windows\Fonts\Deng.ttf", 0),
    (r"C:\Windows\Fonts\simhei.ttf", 0),
    ("/System/Library/Fonts/PingFang.ttc", 0),
    ("/System/Library/Fonts/STHeiti Light.ttc", 0),
    ("/System/Library/Fonts/Supplemental/Songti.ttc", 0),
];

/// 注册在 [`FontDefinitions`] 里的字体名。
///
/// 这个名字只在本进程内有效，和字体文件里的 family 名无关。
const CJK_FONT_NAME: &str = "cjk-system";

/// 给 egui 装上中文字体，并顺手统一各文本样式的字号。
///
/// 必须在使用任何 `ui.label(...)` 之前调用，否则第一帧的中文会先渲染成方块。
///
/// 具体做法是把找到的中文字体插入 `Proportional`（比例字体）和 `Monospace`
/// （等宽字体）两个字族的**最前面**，让它成为首选字体：中文由系统字体渲染，
/// 系统字体里缺失的符号再回退到 egui 自带的字体，兼顾中文观感与图标/符号覆盖。
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    match load_first_available() {
        Some((path, data)) => {
            fonts
                .font_data
                .insert(CJK_FONT_NAME.to_owned(), Arc::new(data));

            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    // 插到索引 0：优先级最高
                    .insert(0, CJK_FONT_NAME.to_owned());
            }

            log::info!("已加载系统中文字体：{path}");
        }
        None => {
            // 不中断启动：界面仍然可用，只是中文会显示为方块
            log::warn!("未找到可用的系统中文字体，界面中文可能显示为方块");
        }
    }

    ctx.set_fonts(fonts);
    apply_text_styles(ctx);
}

/// 从系统目录里找第一份可读的中文字体。
///
/// 返回值里的 `String` 是实际命中的路径，仅用于日志；字体数据本身是
/// [`FontData`]，直接交给 egui 解析。
///
/// 遍历过程中“文件不存在”属于正常情况（另一个平台的路径永远不存在），
/// 因此只记录 `debug` 级别日志；只有全部候选都失败时调用方才会打警告。
fn load_first_available() -> Option<(String, FontData)> {
    for (path, face_index) in CANDIDATES {
        match std::fs::read(path) {
            Ok(bytes) => {
                // `from_blob` 允许指定字体集合内的 face 索引，.ttc 必须用它；
                // 用 `from_owned` 只会解析第 0 个 face，遇到 ttc 的部分子字体可能错位。
                let data = FontData::from_blob(Arc::new(bytes), *face_index);
                return Some(((*path).to_owned(), data));
            }
            Err(err) => log::debug!("跳过候选字体 {path}：{err}"),
        }
    }
    None
}

/// 统一各文本样式的字号。
///
/// egui 默认的正文字号偏小（13pt），中文在托盘气泡这种小窗口里更显拥挤，
/// 这里整体上调一档，并把按钮字号和正文对齐，避免按钮文字比正文小。
fn apply_text_styles(ctx: &egui::Context) {
    // 深色 / 浅色两套主题都要改：`all_styles_mut` 会同时作用于它们，
    // 否则用户切换系统主题后字号会跳回默认值。
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(18.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(12.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(13.0, FontFamily::Monospace),
            ),
        ]
        .into();
    });
}
