//! 托盘面板窗口：显示 / 隐藏 / 定位 / 绘制。
//!
//! 面板是一个**无边框、置顶、不在任务栏显示**的小窗口，行为对标常见的菜单栏工具：
//!
//! * 点击托盘图标 → 在图标旁边弹出面板；
//! * 点击面板外部（窗口失焦）→ 自动收起；
//! * 按 `Esc` → 收起；
//! * 右键托盘菜单里的“退出” → 结束进程。
//!
//! 本文件同时承担“状态机”和“界面绘制”两件事，因为面板目前非常简单；
//! 等账户列表、设置页加进来之后，绘制部分可以整体挪到 `ui` 模块，届时只需搬代码。

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::fonts;
use crate::tray::{TrayBridge, TrayCommand, TrayHandle};

/// 面板逻辑宽度（单位：egui 点；100% 缩放下等于像素）。
pub const PANEL_WIDTH: f32 = 360.0;

/// 面板逻辑高度（单位：egui 点）。
pub const PANEL_HEIGHT: f32 = 460.0;

/// 面板与托盘图标之间留出的空隙，避免气泡紧贴图标。
const TRAY_GAP: f32 = 8.0;

/// 面板与屏幕边缘之间保留的最小距离。
const SCREEN_MARGIN: f32 = 8.0;

/// 面板刚弹出后的“宽限期”。
///
/// 窗口从“显示”到“真正拿到焦点”之间有一两帧延迟，这段时间里
/// `viewport().focused` 可能仍然是 `false`。如果立刻按失焦处理，面板会刚弹出就消失，
/// 所以在这段时间内不响应失焦事件。
const FOCUS_GRACE: Duration = Duration::from_millis(400);

/// 收起面板后的去抖窗口。
///
/// 用户点击托盘图标关闭面板时，同一动作会先让面板失焦（触发自动收起），
/// 再派发一次托盘单击事件（触发“切换显示”）。若不去抖，面板会关掉又立刻弹回来。
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(250);

/// 面板背景色（深色，接近 sub2api logo 的深蓝黑）。
const PANEL_BG: egui::Color32 = egui::Color32::from_rgb(15, 23, 42);

/// 面板边框色，用来在浅色桌面上勾勒出气泡的轮廓。
const PANEL_BORDER: egui::Color32 = egui::Color32::from_rgb(51, 65, 85);

/// 卡片（状态区）背景色。
const CARD_BG: egui::Color32 = egui::Color32::from_rgb(30, 41, 59);

/// 空状态占位区的背景色，比卡片再暗一点，形成层次。
const EMPTY_BG: egui::Color32 = egui::Color32::from_rgb(22, 33, 51);

/// 主要文字颜色。
const TEXT_PRIMARY: egui::Color32 = egui::Color32::from_rgb(226, 232, 240);

/// 次要文字颜色（说明文字、版本号、快捷键提示）。
const TEXT_SECONDARY: egui::Color32 = egui::Color32::from_rgb(148, 163, 184);

/// 更弱的文字颜色，用于占位说明一类的“可读但不打扰”的内容。
const TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(100, 116, 139);

/// 品牌强调色（取自 sub2api logo 的薄荷绿渐变起始色）。
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(121, 244, 189);

/// 应用主体。
pub struct TrayApp {
    /// 托盘图标句柄。**必须持有**：drop 之后托盘图标会消失。
    tray: TrayHandle,

    /// 与托盘回调通信的桥，每帧从中取指令。
    bridge: Arc<TrayBridge>,

    /// 面板当前是否应当可见（这是应用侧的期望值，不是操作系统的实际值）。
    panel_visible: bool,

    /// 面板本次被弹出的时刻，用于计算 [`FOCUS_GRACE`] 宽限期。
    shown_at: Option<Instant>,

    /// 面板上一次被收起的时刻，用于 [`TOGGLE_DEBOUNCE`] 去抖。
    last_hidden_at: Option<Instant>,

    /// 是否还在第一帧。
    ///
    /// eframe 会先显示窗口再跑第一帧（历史上是为了避免白屏闪烁），
    /// 托盘程序启动时不应该出现面板，所以第一帧里立刻把它收起来。
    first_frame: bool,

    /// 调试开关：进程启动时若存在环境变量 `SUB2API_TRAY_DEBUG_VISIBLE`，
    /// 就直接弹出面板。
    ///
    /// 之所以需要它：托盘程序平时启动后处于“隐身”状态，自动化冒烟测试和
    /// 截图验收都没办法去点托盘图标；有了这个开关就能稳定复现“面板已打开”的画面。
    /// 正常使用时不设置该变量，行为与正式版本完全一致。
    debug_show_on_start: bool,
}

impl TrayApp {
    /// 创建应用：装字体、设配色、建托盘。
    ///
    /// 这个函数在 winit 事件循环内部被调用，此时主线程与事件循环都已就绪，
    /// 满足 `tray-icon` 对“必须先有事件循环”的要求。
    pub fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
        // 中文字体必须在任何界面绘制之前装好，否则第一帧会是方块
        fonts::install(&cc.egui_ctx);

        // 深色配色：托盘气泡贴在深色任务栏旁边时观感更统一。
        // 这里直接改“两套主题”的样式，而不是只改当前主题：否则用户把系统切到浅色后，
        // 按钮、分隔线等控件会变回浅色，和固定的深色面板底色打架。
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = PANEL_BG;
        visuals.window_fill = PANEL_BG;
        cc.egui_ctx
            .all_styles_mut(|style| style.visuals = visuals.clone());

        let bridge = TrayBridge::new(cc.egui_ctx.clone());
        let tray = crate::tray::install(Arc::clone(&bridge))?;

        let debug_show_on_start = std::env::var_os("SUB2API_TRAY_DEBUG_VISIBLE").is_some();
        if debug_show_on_start {
            log::warn!("检测到 SUB2API_TRAY_DEBUG_VISIBLE：启动后直接显示面板（仅用于调试）");
        }

        Ok(Self {
            tray,
            bridge,
            panel_visible: false,
            shown_at: None,
            last_hidden_at: None,
            first_frame: true,
            debug_show_on_start,
        })
    }

    /// 处理本帧从托盘收到的指令。
    fn handle_tray_commands(&mut self, ctx: &egui::Context) {
        for command in self.bridge.drain() {
            match command {
                TrayCommand::TogglePanel => {
                    if self.panel_visible {
                        self.hide_panel(ctx);
                    } else if !self.recently_hidden() {
                        self.show_panel(ctx);
                    } else {
                        // 刚因失焦而收起：这次点击的真实语义是“关闭”，不再重新弹出
                        log::debug!("忽略去抖窗口内的托盘点击");
                    }
                }
                TrayCommand::ShowPanel => self.show_panel(ctx),
                TrayCommand::Quit => {
                    log::info!("收到退出指令，关闭应用");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// 处理“点击面板外部自动收起”。
    ///
    /// `viewport().focused` 在部分平台可能拿不到（返回 `None`），此时按“仍然聚焦”处理，
    /// 宁可面板不自动收起，也不要让它无法使用。
    fn handle_focus(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if !self.panel_visible || focused {
            return;
        }

        // 宽限期内不响应：窗口刚弹出时 focused 可能还没更新
        let in_grace = self
            .shown_at
            .map(|at| at.elapsed() < FOCUS_GRACE)
            .unwrap_or(false);
        if !in_grace {
            log::debug!("面板失焦，自动收起");
            self.hide_panel(ctx);
        }
    }

    /// 弹出面板：先把窗口移到托盘图标旁边，再显示并抢焦点。
    fn show_panel(&mut self, ctx: &egui::Context) {
        // 顺序很重要：先移动、再显示，用户就不会看到面板从上一个位置“飞”过来
        let position = self.panel_position(ctx);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        // 主动聚焦：面板只有拿到焦点，才能在用户点击别处时收到失焦事件并自动收起
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);

        self.panel_visible = true;
        self.shown_at = Some(Instant::now());
        // 立刻请求重绘，避免等下一帧才看到面板
        ctx.request_repaint();
    }

    /// 收起面板。
    ///
    /// 只把窗口设为不可见，不销毁窗口、也不退出进程——进程要继续待在托盘里。
    fn hide_panel(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.panel_visible = false;
        self.shown_at = None;
        self.last_hidden_at = Some(Instant::now());
    }

    /// 是否刚在去抖窗口内收起过面板。
    fn recently_hidden(&self) -> bool {
        self.last_hidden_at
            .map(|at| at.elapsed() < TOGGLE_DEBOUNCE)
            .unwrap_or(false)
    }

    /// 计算面板左上角应该放在哪里（单位：egui 逻辑点）。
    ///
    /// 这里只做“把平台数据翻译成统一输入”的活：托盘图标矩形来自操作系统，单位是
    /// **物理像素**，而 egui 的窗口位置用**逻辑点**。在 125% / 150% 缩放的 Windows 上
    /// 两者相差 1.25 / 1.5 倍，直接混用会让面板明显偏移，所以先换算再交给
    /// [`compute_panel_position`] 这个纯函数处理（纯函数才能写单元测试）。
    fn panel_position(&self, ctx: &egui::Context) -> egui::Pos2 {
        let pixels_per_point = ctx.pixels_per_point().max(0.1);

        // 当前显示器的逻辑尺寸，用来把面板夹回屏幕内。
        // 拿不到时用一个保守的默认值，保证面板不会被算到负数坐标去。
        let monitor = ctx
            .input(|i| i.viewport().monitor_size)
            .unwrap_or_else(|| egui::vec2(1280.0, 800.0));

        let icon = self.tray.rect().map(|rect| IconBounds {
            left: rect.position.x as f32 / pixels_per_point,
            top: rect.position.y as f32 / pixels_per_point,
            right: (rect.position.x + f64::from(rect.size.width)) as f32 / pixels_per_point,
            bottom: (rect.position.y + f64::from(rect.size.height)) as f32 / pixels_per_point,
        });

        compute_panel_position(icon, monitor, egui::vec2(PANEL_WIDTH, PANEL_HEIGHT))
    }

    /// 绘制整个面板。
    ///
    /// `ui` 是 eframe 提供的根 Ui（没有边距也没有背景），因此这里用 [`egui::CentralPanel`]
    /// 再包一层，才能得到带圆角/内边距的气泡外观。
    fn draw_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Esc 收起：托盘气泡的通用习惯，也方便键盘用户
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.hide_panel(ctx);
            return;
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    // 圆角 + 深色填充 + 细边框，构成气泡本体
                    .fill(PANEL_BG)
                    .corner_radius(egui::CornerRadius::same(12))
                    .stroke(egui::Stroke::new(1.0, PANEL_BORDER))
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ui, |ui| {
                draw_header(ui);
                ui.add_space(14.0);
                draw_status_card(ui);
                ui.add_space(12.0);
                draw_primary_action(ui);
                ui.add_space(16.0);
                draw_empty_state(ui);
                // 把页脚推到底部：面板高度固定，内容不足时也不会全部挤在顶部
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    draw_footer(ui, ctx);
                });
            });
    }
}

impl eframe::App for TrayApp {
    /// 清屏色设为全透明。
    ///
    /// 面板自己绘制带圆角的背景，窗口四角的像素必须保持透明，
    /// 否则会露出一块方形的底色，圆角就白做了。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    /// 每帧的“逻辑阶段”：处理托盘指令与焦点变化，不做任何绘制。
    ///
    /// eframe 在面板隐藏时仍会调用这个函数（此时通过 `request_repaint` 唤醒），
    /// 因此“隐藏状态下点击托盘显示面板”的链路是通的；
    /// 而 [`eframe::App::ui`] 只在真正需要绘制时才被调用。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.first_frame {
            self.first_frame = false;
            if self.debug_show_on_start {
                // 调试模式：启动就弹出面板，方便截图与手工验收
                self.show_panel(ctx);
            } else {
                // 启动后立刻收起：托盘程序不该在启动时弹出一个空面板
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }

        self.handle_tray_commands(ctx);
        self.handle_focus(ctx);
    }

    /// 每帧的“绘制阶段”：只有面板应当可见时才画。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.panel_visible {
            return;
        }

        // 从根 Ui 里拿到 Context 的副本（内部引用计数，克隆很便宜），
        // 这样 draw_panel 既需要 ui 也需要 ctx 时不会产生借用冲突。
        let ctx = ui.ctx().clone();
        self.draw_panel(ui, &ctx);
    }
}

/// 托盘图标在**逻辑点**坐标系下的矩形。
///
/// 把 `tray_icon::Rect`（物理像素）换算后包成这个结构，是为了让定位逻辑可以
/// 脱离平台 API 单独测试。
#[derive(Debug, Clone, Copy, PartialEq)]
struct IconBounds {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl IconBounds {
    /// 图标中心的纵坐标，用来判断图标位于屏幕上半部分还是下半部分。
    fn center_y(&self) -> f32 {
        (self.top + self.bottom) * 0.5
    }
}

/// 纯函数：算出面板左上角的逻辑坐标。
///
/// 规则：
///
/// 1. 右边缘对齐图标右边缘（托盘图标基本都贴在屏幕右侧，这样面板不会越界）；
/// 2. 图标在屏幕上半部分（macOS 菜单栏）→ 面板放在图标下方；
///    图标在下半部分（Windows 任务栏）→ 面板放在图标上方，避免被屏幕底边切掉；
/// 3. 最后把坐标夹回屏幕可见范围，并给屏幕边缘留 [`SCREEN_MARGIN`] 的间隙；
/// 4. 拿不到图标矩形时，按平台习惯给出兜底位置（见 [`fallback_position`]）。
fn compute_panel_position(
    icon: Option<IconBounds>,
    monitor: egui::Vec2,
    panel: egui::Vec2,
) -> egui::Pos2 {
    let (x, y) = match icon {
        Some(icon) => {
            let place_above = icon.center_y() > monitor.y * 0.5;
            (
                icon.right - panel.x,
                if place_above {
                    icon.top - panel.y - TRAY_GAP
                } else {
                    icon.bottom + TRAY_GAP
                },
            )
        }
        None => fallback_position(monitor, panel),
    };

    // 夹回屏幕内。`max(SCREEN_MARGIN)` 处理“面板比屏幕还大”这种极端情况，
    // 避免出现 min > max 导致 clamp panic。
    let max_x = (monitor.x - panel.x - SCREEN_MARGIN).max(SCREEN_MARGIN);
    let max_y = (monitor.y - panel.y - SCREEN_MARGIN).max(SCREEN_MARGIN);

    egui::pos2(x.clamp(SCREEN_MARGIN, max_x), y.clamp(SCREEN_MARGIN, max_y))
}

/// 拿不到托盘图标位置时的兜底定位。
///
/// * macOS：菜单栏在屏幕顶部，面板贴在右上角往下一点；
/// * Windows：任务栏默认在屏幕底部，面板贴在右下角往上一点（额外让开任务栏高度）。
fn fallback_position(monitor: egui::Vec2, size: egui::Vec2) -> (f32, f32) {
    #[cfg(target_os = "macos")]
    {
        (monitor.x - size.x - SCREEN_MARGIN, 32.0)
    }

    #[cfg(not(target_os = "macos"))]
    {
        (
            monitor.x - size.x - SCREEN_MARGIN,
            monitor.y - size.y - SCREEN_MARGIN - 48.0,
        )
    }
}

/// 绘制标题区：产品名 + 版本号 + 一句话说明。
fn draw_header(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Sub2API 托盘")
                .size(16.0)
                .strong()
                .color(TEXT_PRIMARY),
        );

        // 版本号靠右显示，弱化处理，避免抢标题的视觉重心
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(format!("v{}", crate::APP_VERSION))
                    .size(11.0)
                    .color(TEXT_SECONDARY),
            );
        });
    });

    ui.add_space(2.0);
    ui.label(
        egui::RichText::new("在托盘里查看已登录账户的状态")
            .size(11.0)
            .color(TEXT_SECONDARY),
    );

    // 标题下的一条品牌色细线：用 sub2api logo 的薄荷绿做点缀，
    // 让面板在深色任务栏旁边也能一眼认出是哪个产品。
    ui.add_space(8.0);
    let (line_rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 2.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(line_rect, egui::CornerRadius::same(1), ACCENT);
}

/// 绘制实例登录状态卡片。
///
/// 目前是占位内容；实例配置与登录接入后，这里会显示实例地址与连接状态。
fn draw_status_card(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(CARD_BG)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            // 撑满宽度，否则卡片会按内容收缩，看起来像个小方块
            ui.set_width(ui.available_width());

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("●").size(12.0).color(TEXT_SECONDARY));
                ui.label(
                    egui::RichText::new("尚未登录任何实例")
                        .size(14.0)
                        .strong()
                        .color(TEXT_PRIMARY),
                );
            });

            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("配置 Sub2API 实例地址并登录后，托盘会定时拉取账户状态。")
                    .size(12.0)
                    .color(TEXT_SECONDARY),
            );
        });
}

/// 绘制账户列表的占位区。
///
/// 面板高度是固定的，账户列表接入前这里必须有点内容，否则面板下半截会是一大片空白。
/// 等真实列表做好后，直接把这个函数替换成滚动列表即可。
fn draw_empty_state(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(EMPTY_BG)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("账户状态列表")
                        .size(12.0)
                        .color(TEXT_SECONDARY),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("登录后按账户展示在线状态、剩余额度与最近使用时间。")
                        .size(11.0)
                        .color(TEXT_DIM),
                );
                ui.add_space(6.0);
            });
        });
}

/// 绘制主操作区。
///
/// “登录账户”暂时禁用：按钮先占位，避免等登录流程做好后界面结构大改。
fn draw_primary_action(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.add_enabled(false, egui::Button::new("登录账户"));
        ui.label(
            egui::RichText::new("登录功能开发中")
                .size(11.0)
                .color(TEXT_SECONDARY),
        );
    });
}

/// 绘制页脚：操作提示 + 退出按钮。
fn draw_footer(ui: &mut egui::Ui, ctx: &egui::Context) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("左键托盘图标显示 / 隐藏面板 · Esc 收起")
                .size(11.0)
                .color(TEXT_SECONDARY),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("退出").clicked() {
                // 与托盘菜单的“退出”走同一条路径：关闭根视口即结束进程
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常见笔记本分辨率（逻辑点），用于构造测试场景。
    const MONITOR: egui::Vec2 = egui::Vec2::new(1440.0, 900.0);

    fn panel() -> egui::Vec2 {
        egui::vec2(PANEL_WIDTH, PANEL_HEIGHT)
    }

    /// Windows 任务栏场景：图标在屏幕右下角 → 面板应放在图标**上方**，右边缘对齐。
    #[test]
    fn 图标在右下角时面板向右上展开() {
        let icon = IconBounds {
            left: 1390.0,
            top: 862.0,
            right: 1414.0,
            bottom: 886.0,
        };

        let pos = compute_panel_position(Some(icon), MONITOR, panel());

        assert_eq!(pos.x, icon.right - PANEL_WIDTH);
        assert_eq!(pos.y, icon.top - PANEL_HEIGHT - TRAY_GAP);
    }

    /// macOS 菜单栏场景：图标在屏幕右上角 → 面板应放在图标**下方**。
    #[test]
    fn 图标在右上角时面板向下展开() {
        let icon = IconBounds {
            left: 1380.0,
            top: 0.0,
            right: 1404.0,
            bottom: 24.0,
        };

        let pos = compute_panel_position(Some(icon), MONITOR, panel());

        assert_eq!(pos.x, icon.right - PANEL_WIDTH);
        assert_eq!(pos.y, icon.bottom + TRAY_GAP);
    }

    /// 图标贴着屏幕左边时，面板不应跑到屏幕外，而应被夹到左边距处。
    #[test]
    fn 图标靠左时面板被夹回屏幕内() {
        let icon = IconBounds {
            left: 4.0,
            top: 400.0,
            right: 28.0,
            bottom: 424.0,
        };

        let pos = compute_panel_position(Some(icon), MONITOR, panel());

        assert_eq!(pos.x, SCREEN_MARGIN);
        // 纵向仍在图标下方，未触发夹取
        assert_eq!(pos.y, icon.bottom + TRAY_GAP);
    }

    /// 拿不到图标矩形时，面板必须仍然落在屏幕内（且不出现负坐标）。
    #[test]
    fn 缺少图标信息时使用兜底位置() {
        let pos = compute_panel_position(None, MONITOR, panel());

        assert!(pos.x >= SCREEN_MARGIN);
        assert!(pos.y >= SCREEN_MARGIN);
        assert!(pos.x + PANEL_WIDTH <= MONITOR.x - SCREEN_MARGIN);
        assert!(pos.y + PANEL_HEIGHT <= MONITOR.y - SCREEN_MARGIN);
    }

    /// 屏幕比面板还小的极端情况不应该 panic（clamp 的上下界会重叠）。
    #[test]
    fn 屏幕过小时不会崩溃() {
        let tiny = egui::vec2(200.0, 200.0);
        let pos = compute_panel_position(None, tiny, panel());

        assert!(pos.x.is_finite());
        assert!(pos.y.is_finite());
    }
}
