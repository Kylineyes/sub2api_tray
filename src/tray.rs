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
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, Rect as TrayRect, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

/// 内嵌到二进制里的托盘图标（PNG）。
///
/// 用 `include_bytes!` 而不是运行时读文件，目的是让“复制一个 exe 就能跑”成立，
/// 不依赖安装目录结构。图标本身是 128×128，交给系统缩放到 16/20/24 像素。
const TRAY_ICON_PNG: &[u8] = include_bytes!("../assets/tray-icon.png");

/// 菜单项 id：显示面板。
const MENU_ID_SHOW: &str = "show";

/// 菜单项 id：打开登录弹窗。
const MENU_ID_LOGIN: &str = "login";

/// 菜单项 id：退出程序。
const MENU_ID_QUIT: &str = "quit";

/// 账号子菜单里"退出账号并删除记录"的 id 前缀。
///
/// 形如 `delete:<uuid>`；菜单重建后 id 会跟着账号变化，所以回调里按前缀解析，
/// 而不是在安装时把每个 id 都捕获进闭包。
const MENU_ID_DELETE_PREFIX: &str = "delete:";

/// 托盘需要应用做的事。
///
/// 这里不再是 `Copy`：删除账号要带上 id。投递成本仍然很低（Vec push）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    /// 切换面板显示状态（左键单击托盘图标）。
    TogglePanel,
    /// 显示面板（菜单项）。
    ShowPanel,
    /// 打开登录弹窗（菜单项，面板未显示时会先显示面板）。
    OpenLogin,
    /// 删除某个账号（子菜单项，id 为账号 id；需要二次确认）。
    DeleteAccount(String),
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

/// 账号子菜单需要的信息（由 `app` 根据运行态生成，托盘不关心状态怎么来的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayAccountEntry {
    /// 账号 id（删除指令要用）。
    pub id: String,
    /// 子菜单标题，例如 `zh***@ex***.com · 在线`。
    pub title: String,
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
/// 结构（与 `docs/plans/0004-account-menu.md` 第 4.1 节一致）：
///
/// ```text
/// 显示面板
/// 登陆新用户…
/// ─────────────
/// <账号 1> ▸ 退出账号并删除记录
/// <账号 2> ▸ 退出账号并删除记录
/// ─────────────
/// 版本 v0.1.0（禁用）
/// ─────────────
/// 退出
/// ```
///
/// `entries` 的顺序由调用方决定（`app` 按 `createdAt` 升序传入，即添加从早到晚）。
pub fn build_menu(entries: &[TrayAccountEntry]) -> Result<Menu> {
    let menu = Menu::new();

    let show_item = MenuItem::with_id(MENU_ID_SHOW, "显示面板", true, None);
    let login_item = MenuItem::with_id(MENU_ID_LOGIN, "登陆新用户…", true, None);
    let version_item = MenuItem::with_id(
        "version",
        format!("版本 v{}", crate::APP_VERSION),
        false,
        None,
    );
    let quit_item = MenuItem::with_id(MENU_ID_QUIT, "退出", true, None);

    menu.append_items(&[&show_item, &login_item, &PredefinedMenuItem::separator()])
        .context("构建托盘菜单失败")?;

    // 每个账号一个子菜单：标题带状态，子菜单里目前只有"退出账号并删除记录"。
    // 将来 API Key 管理也放在这里（见 0004 计划）。
    for entry in entries {
        let submenu = Submenu::new(entry.title.as_str(), true);
        let delete_item = MenuItem::with_id(
            format!("{MENU_ID_DELETE_PREFIX}{}", entry.id),
            "退出账号并删除记录",
            true,
            None,
        );
        submenu.append(&delete_item).context("构建账号子菜单失败")?;
        menu.append(&submenu).context("追加账号子菜单失败")?;
    }

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
    let menu = build_menu(&[])?;

    // ---- 菜单事件 ----
    // 只注册一次：菜单会被整份替换，但 id 规则不变，所以按 id 字符串分发即可。
    let menu_bridge = Arc::clone(&bridge);
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let id = event.id.as_ref();
        match id {
            MENU_ID_SHOW => menu_bridge.push(TrayCommand::ShowPanel),
            MENU_ID_LOGIN => menu_bridge.push(TrayCommand::OpenLogin),
            MENU_ID_QUIT => menu_bridge.push(TrayCommand::Quit),
            other => {
                if let Some(account_id) = other.strip_prefix(MENU_ID_DELETE_PREFIX) {
                    menu_bridge.push(TrayCommand::DeleteAccount(account_id.to_owned()));
                }
                // 版本项是禁用的，不会产生事件；其它未知 id 直接忽略
            }
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
    fn 菜单标题允许中文与状态后缀() {
        // 只验证"标题是可构造的字符串"，不涉及系统菜单对象（那需要事件循环）
        let entry = TrayAccountEntry {
            id: "8f14e45f".to_owned(),
            title: "zh***@ex***.com · 在线".to_owned(),
        };
        assert!(entry.title.contains("在线"));
    }

    #[test]
    fn 删除菜单id可以还原出账号id() {
        let id = format!("{MENU_ID_DELETE_PREFIX}abc-123");
        let restored = id.strip_prefix(MENU_ID_DELETE_PREFIX).expect("前缀应匹配");
        assert_eq!(restored, "abc-123");
    }

    #[test]
    fn 菜单常量互不相同() {
        // 防止将来改常量时把两个入口写成同一个 id（会导致点击行为串台）
        assert_ne!(MENU_ID_SHOW, MENU_ID_LOGIN);
        assert_ne!(MENU_ID_SHOW, MENU_ID_QUIT);
        assert_ne!(MENU_ID_LOGIN, MENU_ID_QUIT);
    }
}
