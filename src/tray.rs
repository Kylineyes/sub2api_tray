//! 系统托盘（Windows 通知区域 / macOS 菜单栏）的封装。
//!
//! 这里只负责"图标 + 菜单 + 事件"，不涉及任何业务状态，也没有界面布局。
//!
//! # 线程模型（重要）
//!
//! `tray-icon` 的事件回调在平台事件循环线程里执行（Windows 上就是 winit 的消息泵线程），
//! 回调里**不能**直接操作窗口或应用状态。因此这里的做法是：
//!
//! 1. 回调只把"用户意图"（[`TrayCommand`]）塞进 [`TrayBridge`] 的队列；
//! 2. 顺带调用 `egui::Context::request_repaint()` 唤醒界面；
//! 3. 下一帧由 `app` 模块取走队列里的指令并真正执行（显示面板、打开登录弹窗、删除账号等）。
//!
//! 面板隐藏时 eframe 依然会以“只跑逻辑、不绘制”的模式执行 `App::logic`，
//! 所以这个唤醒链路在窗口不可见时也是通的。
//!
//! # 菜单是"快照"
//!
//! `muda` 的菜单对象不是 `Send`，只能在主线程创建；而且**一旦创建就无法增删项**。
//! 所以账号变化（新增/删除/状态刷新）时，做法是"整份重建再 `set_menu`"，
//! 而不是去改某个子项。重建只发生在状态真正变化时，不会每帧都做。
//!
//! # 生命周期
//!
//! [`TrayHandle`] 内部持有 `TrayIcon`，**必须一直存活**：
//! 一旦被 drop，图标就会立刻从托盘消失。因此它被保存在 `app::TrayApp` 的字段里，
//! 直到进程退出才释放。

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, Rect as TrayRect, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

/// 内嵌到二进制里的托盘图标（PNG）。
///
/// 用 `include_bytes!` 而不是运行时读文件，目的是让“复制一个 exe 就能跑”成立，
/// 不依赖安装目录结构。图标本身是 128×128，交给系统缩放到 16/20/24 像素。
const TRAY_ICON_PNG: &[u8] = include_bytes!("../assets/tray-icon.png");

/// 菜单项 id：立即刷新状态。
const MENU_ID_REFRESH: &str = "refresh";

/// 菜单项 id：退出程序。
const MENU_ID_QUIT: &str = "quit";

/// 托盘需要应用做的事。
///
/// 这里不再是 `Copy`：删除账号要带上 id。投递成本仍然很低（Vec push）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    /// 切换面板显示状态（左键单击托盘图标）。
    TogglePanel,
    /// 立即刷新一轮状态（菜单项或面板按钮，绕过定时与启动延迟）。
    RefreshNow,
    /// 退出整个应用（菜单项）。
    Quit,
}

/// 托盘与 egui 之间的通信桥。
///
/// 之所以要单独抽一个结构体，是因为托盘回调要求 `Fn + Send + Sync + 'static`，
/// 而 `egui::Context` 本身是 `Clone + Send + Sync` 的，可以直接放进桥里用来唤醒界面。
pub struct TrayBridge {
    /// egui 的上下文，用来请求重绘（唤醒事件循环）
    ctx: egui::Context,
    /// 待处理的指令队列；由托盘线程写入、界面线程读取
    pending: Mutex<Vec<TrayCommand>>,
}

impl TrayBridge {
    /// 创建一个通信桥。
    ///
    /// 传入的 `ctx` 必须是界面线程那份 `egui::Context`（克隆开销极小，内部是 Arc）。
    pub fn new(ctx: egui::Context) -> Arc<Self> {
        Arc::new(Self {
            ctx,
            pending: Mutex::new(Vec::new()),
        })
    }

    /// 投递一条指令，并唤醒界面线程。
    ///
    /// 这个方法可以从任意线程调用，包括托盘回调所在的事件循环线程。
    pub fn push(&self, command: TrayCommand) {
        // 互斥锁“中毒”意味着另一个线程在持锁期间 panic 了。
        // 此时队列本身仍然可用，所以恢复内部数据继续投递，而不是让托盘彻底失灵。
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.push(command);

        // 唤醒 egui：即使面板当前不可见，eframe 也会执行一次“只跑逻辑”的 update。
        self.ctx.request_repaint();
    }

    /// 取出当前所有待处理指令。
    ///
    /// 用 `mem::take` 一次性搬空队列，避免在处理过程中反复加锁。
    pub fn drain(&self) -> Vec<TrayCommand> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *pending)
    }
}

/// 托盘图标的持有者。
pub struct TrayHandle {
    /// 真正注册到系统里的托盘图标；drop 即从托盘消失
    icon: TrayIcon,
}

impl TrayHandle {
    /// 返回托盘图标在屏幕上的矩形（**物理像素**，坐标系为整个虚拟屏幕）。
    ///
    /// 平台差异：
    ///
    /// * Windows：由 Shell 返回，可靠；
    /// * macOS：菜单栏项的位置通常取不到，会返回 `None`，此时由调用方走兜底定位；
    /// * Linux：不在本期支持范围内。
    pub fn rect(&self) -> Option<TrayRect> {
        self.icon.rect()
    }

    /// 用新菜单替换当前菜单。
    ///
    /// 必须在主线程调用（`muda` 菜单不是 `Send`）。菜单项发生变化（账号增删、
    /// 状态刷新）时由 `app` 重建后调用。
    pub fn set_menu(&self, menu: Menu) {
        self.icon.set_menu(Some(Box::new(menu)));
    }

    /// 更新托盘图标的悬停提示。
    ///
    /// 失败只影响提示文字，不影响功能，因此返回 `Result` 但调用方可以忽略。
    pub fn set_tooltip(&self, text: &str) -> Result<()> {
        self.icon
            .set_tooltip(Some(text))
            .context("更新托盘提示失败")
    }

    /// 更新图标右下角的状态徽标。
    ///
    /// * `Some(rgb)`：在基础图标上合成一个该颜色的圆点；
    /// * `None`：恢复基础图标（没有账号、或状态还不明确时）。
    ///
    /// 只在聚合状态变化时由 `app` 调用：`set_icon` 会触发任务栏重绘，
    /// 每轮都换会闪。失败时保留旧图标，不影响功能。
    pub fn set_badge(&self, rgb: Option<[u8; 3]>) -> Result<()> {
        let (mut rgba, width, height) = load_icon_rgba()?;
        if let Some(rgb) = rgb {
            compose_badge(&mut rgba, width, height, rgb);
        }
        let icon = Icon::from_rgba(rgba, width, height).context("合成状态徽标失败")?;
        self.icon.set_icon(Some(icon)).context("更新托盘图标失败")
    }
}

/// 在 RGBA 缓冲的右下角画一个带深色描边的实心圆点。
///
/// 参数：
///
/// * `rgba`：`width * height * 4` 的像素缓冲，就地修改；
/// * `width` / `height`：图标尺寸；
/// * `rgb`：圆点颜色。
///
/// 圆点半径取 `width / 5`（128 图标即 25px），外圈再画 2px 深色描边——
/// 托盘背景可能是浅色也可能是深色，没有描边的纯色圆点在浅色任务栏上会糊成一片。
///
/// 实现上不做抗锯齿：图标会被系统缩放到 16~24px，硬边缘反而更清晰，
/// 也省掉混合运算。
pub fn compose_badge(rgba: &mut [u8], width: u32, height: u32, rgb: [u8; 3]) {
    let radius = (width / 5).max(6) as i32;
    let ring = radius + 2;
    // 圆点贴着右下角，留 2px 边距，避免被任务栏裁掉
    let center_x = width as i32 - radius - 2;
    let center_y = height as i32 - radius - 2;

    let ring_color = [15_u8, 23, 42];

    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let dx = x - center_x;
            let dy = y - center_y;
            let distance_sq = dx * dx + dy * dy;
            if distance_sq > ring * ring {
                continue;
            }

            let color = if distance_sq <= radius * radius {
                rgb
            } else {
                ring_color
            };

            let index = ((y as u32 * width + x as u32) * 4) as usize;
            rgba[index] = color[0];
            rgba[index + 1] = color[1];
            rgba[index + 2] = color[2];
            rgba[index + 3] = 255;
        }
    }
}

/// 读取并解码内嵌的托盘图标。
///
/// 返回值为 `(RGBA 字节, 宽, 高)`。RGBA 的排列顺序是 R、G、B、A 逐像素排列，
/// 与 `tray_icon::Icon::from_rgba` 和 `egui::IconData` 的要求一致。
pub fn load_icon_rgba() -> Result<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory(TRAY_ICON_PNG)
        .context("托盘图标不是可识别的图片格式")?
        .to_rgba8();
    let (width, height) = image.dimensions();
    Ok((image.into_raw(), width, height))
}

/// 构建整份托盘菜单。
///
/// 菜单只保留"原子动作"：
///
/// ```text
/// 立即刷新
/// ─────────────
/// 版本 v0.1.0（禁用）
/// ─────────────
/// 退出
/// ```
///
/// 面板用左键单击托盘图标切换，登录入口在面板顶部，账号信息也都在面板里，
/// 因此菜单不再提供"显示面板 / 登陆新用户 / 账号子菜单"。
pub fn build_menu() -> Result<Menu> {
    let menu = Menu::new();

    // 菜单只留"原子动作"：面板用左键单击切换、登录在面板顶部，
    // 所以这里不再放"显示面板/登陆新用户"两项。
    let refresh_item = MenuItem::with_id(MENU_ID_REFRESH, "立即刷新", true, None);
    let version_item = MenuItem::with_id(
        "version",
        format!("版本 v{}", crate::APP_VERSION),
        false,
        None,
    );
    let quit_item = MenuItem::with_id(MENU_ID_QUIT, "退出", true, None);

    menu.append(&refresh_item).context("构建托盘菜单失败")?;

    menu.append_items(&[
        &PredefinedMenuItem::separator(),
        &version_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .context("构建托盘菜单尾部失败")?;

    Ok(menu)
}

/// 创建托盘图标、注册菜单事件与点击事件。
///
/// 必须在事件循环已经开始运行、且处于主线程时调用（eframe 的 `AppCreator`
/// 正好满足这个条件）。
///
/// # 菜单设计
///
/// * 左键单击：切换面板（不弹菜单），符合 Windows 托盘工具的常见习惯；
/// * 右键单击：弹出菜单（含账号子菜单）。
pub fn install(bridge: Arc<TrayBridge>) -> Result<TrayHandle> {
    let (rgba, width, height) = load_icon_rgba()?;
    let icon = Icon::from_rgba(rgba, width, height).context("托盘图标 RGBA 数据非法")?;

    // 初始菜单没有账号；`app` 拿到数据层后会立刻用真实账号重建一次。
    let menu = build_menu()?;

    // ---- 菜单事件 ----
    // 只注册一次：菜单会被整份替换，但 id 规则不变，所以按 id 字符串分发即可。
    let menu_bridge = Arc::clone(&bridge);
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let id = event.id.as_ref();
        match id {
            MENU_ID_REFRESH => menu_bridge.push(TrayCommand::RefreshNow),
            MENU_ID_QUIT => menu_bridge.push(TrayCommand::Quit),
            // 版本项是禁用的，不会产生事件；其它未知 id 直接忽略
            _ => {}
        }
    }));

    // ---- 托盘图标事件 ----
    let tray_bridge = Arc::clone(&bridge);
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        // 只关心“左键抬起”。按下/双击/鼠标进出都会产生事件，但都不应该切换面板：
        // * 按下时切换，抬起时可能又触发一次，导致面板闪两下；
        // * 双击由系统先派发两次单击，行为会和双击语义冲突。
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            tray_bridge.push(TrayCommand::TogglePanel);
        }
    }));

    // ---- 创建图标 ----
    let icon_handle = TrayIconBuilder::new()
        // 固定 id：后续要做“多托盘图标”或按 id 定位时，需要一个稳定标识
        .with_id("sub2api-tray")
        .with_icon(icon)
        .with_tooltip("Sub2API 托盘 · 未登录")
        .with_menu(Box::new(menu))
        // 左键留给面板开关，右键才弹菜单
        .with_menu_on_left_click(false)
        .with_menu_on_right_click(true)
        .build()
        .context("创建托盘图标失败")?;

    log::info!("托盘图标已就绪");

    Ok(TrayHandle { icon: icon_handle })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 菜单常量互不相同() {
        // 防止将来改常量时把两个入口写成同一个 id（会导致点击行为串台）
        assert_ne!(MENU_ID_REFRESH, MENU_ID_QUIT);
    }

    /// 构造一个纯色缓冲，便于断言像素。
    fn solid_rgba(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut rgba = vec![0_u8; (width * height * 4) as usize];
        // 按 4 字节步进写像素；比 chunks_exact_mut 更直白，也避免 clippy 的常量块提示
        for index in (0..rgba.len()).step_by(4) {
            rgba[index] = rgb[0];
            rgba[index + 1] = rgb[1];
            rgba[index + 2] = rgb[2];
            rgba[index + 3] = 255;
        }
        rgba
    }

    #[test]
    fn 徽标中心是状态色且角落保持原样() {
        let width = 128_u32;
        let height = 128_u32;
        let base = [10_u8, 20, 30];
        let badge = [121_u8, 244, 189];
        let mut rgba = solid_rgba(width, height, base);

        compose_badge(&mut rgba, width, height, badge);

        // 圆心附近（右下角）应当变成徽标色
        let radius = (width / 5).max(6) as i32;
        let center_x = (width as i32 - radius - 2) as u32;
        let center_y = (height as i32 - radius - 2) as u32;
        let index = ((center_y * width + center_x) * 4) as usize;
        assert_eq!(&rgba[index..index + 3], &badge);

        // 左上角远离徽标，必须保持原样（否则说明圆画大了）
        assert_eq!(&rgba[0..3], &base);

        // 描边：圆心上方 radius+1 处应当是深色环
        let ring_y = (center_y as i32 - radius - 1) as u32;
        let ring_index = ((ring_y * width + center_x) * 4) as usize;
        assert_ne!(&rgba[ring_index..ring_index + 3], &badge);
        assert_eq!(rgba[ring_index + 3], 255, "描边必须不透明");
    }

    #[test]
    fn 徽标在小图标上不越界() {
        // 小图标（例如 32px）也要能安全合成
        let width = 32_u32;
        let height = 32_u32;
        let mut rgba = solid_rgba(width, height, [0, 0, 0]);

        compose_badge(&mut rgba, width, height, [255, 0, 0]);

        assert_eq!(rgba.len(), (width * height * 4) as usize);
        assert_eq!(rgba[3], 255, "alpha 通道应保持有效");
    }
}
