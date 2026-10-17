# Sub2API 托盘工具（sub2api_tray）

在 **Windows 通知区域 / macOS 菜单栏** 里常驻一个小图标，点开就能看到已登录账户的状态，
不必打开浏览器登录 Sub2API 的 Web 管理后台。

![面板预览](docs/panel-preview.png)

> 当前处于骨架阶段：托盘、面板、中文字体、开关逻辑已就绪，账户数据尚未接入。

## 当前进度

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| 0 | 托盘图标 + 右键菜单 + 可弹出/收起的面板 | ✅ 已完成 |
| 1 | 配置实例地址、登录、拉取账户与用量 | ⏳ 待规划 |
| 2 | 定时刷新、托盘图标状态着色 | ⏳ 待规划 |
| 3 | 设置页、开机自启、单实例、自动更新 | ⏳ 待规划 |

## 已具备的能力

* 托盘图标 + 右键菜单（显示面板 / 版本信息 / 退出）；
* 左键单击托盘切换面板，右键弹出菜单；
* 无边框、半透明圆角、置顶、不在任务栏显示的面板窗口；
* 面板自动吸附到托盘图标旁（Windows 在图标上方，macOS 在图标下方），
  并会自动避开屏幕边缘；
* 点击面板外部或按 `Esc` 自动收起；
* 界面中文正常渲染（运行时加载系统中文字体，不额外分发字体文件）。

## 构建与运行

前置条件：

* Rust 1.95 及以上（edition 2024）；
* Windows：MSVC 工具链（Visual Studio Build Tools 的“使用 C++ 的桌面开发”）；
* macOS：Xcode Command Line Tools。

```bash
cargo run                 # 调试运行：启动后只有托盘图标，面板默认隐藏
cargo build --release     # 发布构建
```

调试开关：

```powershell
# Windows：启动即显示面板（便于截图/验收）
$env:SUB2API_TRAY_DEBUG_VISIBLE = "1"; cargo run
# 打开调试日志
$env:RUST_LOG = "debug"; cargo run
```

```bash
# macOS
SUB2API_TRAY_DEBUG_VISIBLE=1 cargo run
RUST_LOG=debug cargo run
```

## 目录结构

```
sub2api_tray/
├── AGENTS.md             # 开发约定（领域定位、注释规范、分支与提交流程）
├── docs/plans/           # 功能计划与验收标准（每次新功能先写计划）
├── assets/tray-icon.png  # 托盘图标（内嵌进二进制）
└── src/
    ├── main.rs           # 入口与窗口初始形态
    ├── app.rs            # 面板状态机与界面绘制
    ├── tray.rs           # 托盘图标、菜单、事件桥
    └── fonts.rs          # 系统中文字体加载
```

## 开发约定（摘要）

完整约定见 [AGENTS.md](AGENTS.md)，几个关键点：

* 所有代码注释使用简体中文，并写清“为什么”；
* `main` 只由维护者合入，功能在独立分支完成后合入 `dev`；
* 每个功能先写 `docs/plans/NNNN-xxx.md`（完成点 + 验收标准）再写代码；
* 提交信息遵循约定式提交（Conventional Commits），描述使用中文。

## 已知限制

* Linux 不在支持范围内；
* macOS 尚未在实机上验收；
* Windows 11 默认会把新托盘图标放进“溢出区”，需要在系统设置里手动固定到任务栏；
* 托盘图标目前是占位图，正式图标待定。

## 许可证

[MIT](LICENSE)
