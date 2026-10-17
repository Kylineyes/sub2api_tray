//! 系统托盘（Windows 通知区域 / macOS 菜单栏）的封装。
//!
//! 这里只负责“图标 + 菜单 + 事件”，不涉及任何业务状态，也没有界面布局。
//!
//! # 线程模型（重要）
//!
//! `tray-icon` 的事件回调在平台事件循环线程里执行（Windows 上就是 winit 的消息泵线程），
//! 回调里**不能**直接操作窗口或应用状态。因此这里的做法是：
//!
//! 1. 回调只把“用户意图”（[`TrayCommand`]）塞进 [`TrayBridge`] 的队列；
//! 2. 顺带调用 `egui::Context::request_repaint()` 唤醒界面；
//! 3. 下一帧由 `app` 模块取走队列里的指令并真正执行（显示面板、退出等）。
//!
//! 面板隐藏时 eframe 依然会以“只跑逻辑、不绘制”的模式执行 `App::logic`，
//! 所以这个唤醒链路在窗口不可见时也是通的。
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

/// 托盘需要应用做的事。
///
/// 这是一个很小的枚举，故意不携带复杂数据：托盘回调只做“投递”，
/// 具体怎么处理（要不要定位、要不要抢焦点）留给 `app` 模块决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// 切换面板显示状态（左键单击托盘图标）。
    TogglePanel,
    /// 显示面板（右键菜单里的“显示面板”）。
    ShowPanel,
    /// 退出整个应用（右键菜单里的“退出”）。
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
///
/// 只有两个职责：保持图标存活，以及对外暴露图标在屏幕上的位置（用于面板定位）。
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

/// 创建托盘图标、注册右键菜单与事件回调。
///
/// 必须在事件循环已经开始运行、且处于主线程时调用（eframe 的 `AppCreator`
/// 正好满足这个条件）。
///
/// # 菜单设计
///
/// * 左键单击：切换面板（不弹菜单），符合 Windows 托盘工具的常见习惯；
/// * 右键单击：弹出菜单，包含“显示面板”“版本信息（禁用态）”“退出”。
pub fn install(bridge: Arc<TrayBridge>) -> Result<TrayHandle> {
    let (rgba, width, height) = load_icon_rgba()?;
    let icon = Icon::from_rgba(rgba, width, height).context("托盘图标 RGBA 数据非法")?;

    // ---- 菜单 ----
    let menu = Menu::new();
    let show_item = MenuItem::new("显示面板", true, None);
    // 版本号做成禁用项，纯展示用途；放在分隔线之间，避免误点
    let version_item = MenuItem::new(format!("版本 v{}", crate::APP_VERSION), false, None);
    let quit_item = MenuItem::new("退出", true, None);
    menu.append_items(&[
        &show_item,
        &PredefinedMenuItem::separator(),
        &version_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .context("构建托盘菜单失败")?;

    // ---- 菜单事件 ----
    // 先克隆出菜单 id 再 move 进闭包：菜单项本身要交给托盘持有，不能一并移走。
    let menu_bridge = Arc::clone(&bridge);
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            menu_bridge.push(TrayCommand::ShowPanel);
        } else if event.id == quit_id {
            menu_bridge.push(TrayCommand::Quit);
        }
        // 其他菜单项（例如版本信息）是禁用的，不会产生事件，直接忽略即可
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
