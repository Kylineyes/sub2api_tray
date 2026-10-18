# AGENTS.md — Sub2API 托盘工具开发约定

> 本文件是本仓库的**唯一权威开发约定**。人类开发者与 AI 代理都按本文执行；
> 若其他文档（README、历史注释、聊天记录）与本文冲突，以本文为准。
>
> 修订本文必须单独提交，并在[文末的变更记录](#十三变更记录)中登记。

---

## 一、工程定位

`sub2api_tray` 是一个**常驻系统托盘**的桌面小工具：

* **Windows**：运行在通知区域（任务栏右下角）；
* **macOS**：运行在菜单栏（menu bar）。

核心作用：**在托盘状态下展示已登录账户的状态**——用户不需要打开浏览器、也不需要
登录 Sub2API 的 Web 管理后台，就能随时确认“账号还在不在线、额度还剩多少”。

当前阶段目标（渐进式）：

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| 0 | 工程骨架：托盘图标 + 右键菜单 + 可弹出/收起的面板窗口 | ✅ 已完成（0001） |
| 1a | 数据层：`app.json`、`accounts.json`、凭据加密、账号名打码 | ✅ 已完成（0002，待合入 dev） |
| 1b | 登录：面板顶部入口、登录弹窗、2FA、错误分类 | ✅ 已完成（0003，待合入 dev） |
| 1c | 账号状态刷新与删除确认（账号入口已从原生菜单移入面板） | ✅ 已完成（0004/0011） |
| 1d | API Key 查看与复制（只读，摘要缓存进 accounts.json） | ✅ 已完成（0006，待合入 dev） |
| 1e | 面板内展开账号 Key 明细、10 账号上限、隐藏数据目录 | ✅ 已完成（0007，待合入 dev） |
| 2 | 托盘图标状态化（在线/离线/异常着色）、手动刷新 | ✅ 已完成（0005，待合入 dev） |
| 3 | 设置页、开机自启、单实例、自动更新 | ✅ 已完成（0009/0010；自动更新仅做版本检查，不自动安装） |

**现阶段明确的非目标**（不要顺手做）：

* 不支持 Linux（`tray-icon` 虽支持，但没有验收环境，先不承诺）；
* 不重复实现 Web 管理后台的功能（不做账号增删改、不做支付、不做渠道配置）；
* 不做多语言（界面文案统一简体中文，后续如有需要再单独规划）。

---

## 二、技术栈

| 领域 | 选型 | 版本 | 约束 |
| --- | --- | --- | --- |
| 语言 | Rust | `rust-version = 1.95`，edition 2024 | Windows 使用 MSVC 工具链 |
| 窗口 / 事件循环 | `eframe` + `egui` | 0.37 | 渲染后端**固定 glow**，原因见下 |
| 系统托盘 | `tray-icon`（内含 `muda` 菜单） | 0.26 | 托盘图标必须在事件循环启动后创建 |
| 图片解码 | `png` | 0.17 | 只解内嵌的那一张图标；比 `image` 省约 5MB 体积 |
| 配置 / 账号序列化 | `serde` + `serde_json` | 1.x | `app.json`、`accounts.json` |
| 实例地址解析 | `url` | 2.5 | 归一化与校验（只允许 http/https） |
| 账号标识 | `uuid`（v4） | 1.x | 每个账号的稳定 id |
| 时间格式 | `time`（RFC3339） | 0.3 | `createdAt` / `tokenExpiresAt` |
| 密文编码 | `base64` | 0.23 | DPAPI 密文与钥匙串引用的文本化 |
| Windows 凭据加密 | `windows-sys`（DPAPI） | 0.61 | 当前用户作用域 + 固定附加熵 |
| macOS 凭据存储 | `security-framework`（钥匙串） | 3.7 | 密文入钥匙串，文件只留引用 |
| HTTP 客户端 | `ureq`（rustls） | 3.4 | 阻塞式，必须在后台线程调用；不依赖 OpenSSL |
| 测试临时目录 | `tempfile`（dev） | 3 | 单元测试不污染 exe 目录 |
| 错误处理 | `anyhow` | 1.0 | 库/边界处加 `context` |
| 日志 | `log` + `env_logger` | 0.4 / 0.11 | 默认 `info`，可用 `RUST_LOG` 覆盖 |

**为什么固定使用 glow（OpenGL）而不是默认的 wgpu：**

Windows 上 wgpu 的交换链拿不到带 alpha 的合成模式（启动日志会打印
`Transparent window was requested, but the active wgpu surface does not support...`），
透明窗口会退化为不透明，面板圆角就露出一块方形底色。
`glow` 走 OpenGL，Windows 与 macOS 都能实现真正的透明窗口。
`Cargo.toml` 同时保留两个后端、运行时显式选 `Renderer::Glow`，便于将来平台条件允许时
按平台切换渲染器。

---

## 三、目录结构与模块职责

```
sub2api_tray/
├── AGENTS.md                 # 本文件：开发约定（唯一权威）
├── README.md                 # 面向使用者的简介与构建说明
├── Cargo.toml                # 依赖、元信息、release profile
├── assets/
│   └── tray-icon.png         # 托盘图标（内嵌进二进制，临时占位图）
├── .github/
│   └── workflows/
│       └── release-windows.yml  # 发布流水线：v* tag → Windows x64 产物挂 Release
├── docs/
│   ├── panel-preview.png     # 面板截图（README 引用）
│   └── plans/
│       ├── TEMPLATE.md       # 功能计划模板
│       ├── 0001-*.md         # 骨架
│       ├── 0002-*.md         # 数据层
│       ├── 0003-*.md         # 登录弹窗
│       └── 0004-*.md         # 托盘账号菜单
└── src/
    ├── main.rs               # 入口：日志、数据层初始化、根视口配置、启动 eframe
    ├── app.rs                # 面板状态机（显示/隐藏/失焦收起）+ 界面绘制
    ├── tray.rs               # 托盘图标、右键菜单、托盘事件 → 应用指令
    ├── fonts.rs              # 运行时加载系统中文字体
    ├── paths.rs              # 数据目录、可写探测、原子写入、损坏隔离
    ├── config.rs             # app.json 的读取、默认值与修复
    ├── accounts.rs           # accounts.json 的 schema、排序、去重、增删
    ├── secrets.rs            # Windows DPAPI / macOS 钥匙串的凭据加解密
    ├── mask.rs               # 账号名打码（只影响展示）
    ├── api.rs                # 与 Sub2API 实例通信：登录、2FA、/auth/me、订阅摘要、错误分类
    ├── login.rs              # 登录弹窗：表单状态机 + 后台登录线程 + 2FA 第二步
    ├── state.rs              # 账号运行态（在线/离线/余额/订阅摘要/最近刷新）
    ├── refresh.rs            # 状态刷新线程 + token 失效恢复链（续期→重登→判损坏）
    ├── data.rs               # 数据层门面，供 app / 登录流程调用
    └── single_instance.rs    # 单实例锁（固定本机端口占位）
```

模块边界（改代码前先确认改在哪个文件里）：

* **托盘相关**（图标、菜单项、点击语义）→ `tray.rs`，不要写进 `app.rs`；
* **窗口行为**（位置、可见性、焦点、快捷键）→ `app.rs`；
* **纯视觉**（文案、颜色、布局）→ `app.rs` 里的 `draw_*` 函数；
* **后续业务**（HTTP、登录态、缓存）新增模块（如 `api.rs`、`store.rs`），
  不要把网络请求塞进 `app.rs` 的 `logic()` 里同步执行。

---

## 四、注释规范（硬性要求）

**所有代码注释一律使用简体中文，并且要写得足够详尽——这是本工程的第一条纪律。**

具体要求：

1. **文件级文档注释**（`//!`）必须说明：这个模块负责什么、为什么单独拆出来、
   与其他模块的边界、涉及的关键平台差异。参考 `src/tray.rs` 顶部的写法。
2. **公开项**（`pub fn` / `pub struct` / `pub enum` / 常量）必须用 `///` 说明：
   * 做什么；
   * 为什么这样做（设计动因）；
   * 参数、返回值、副作用；
   * 平台差异（Windows / macOS 行为不同时逐条列出）。
3. **行内注释**（`//`）解释“**为什么**”，而不是复述“这行在做什么”。
   反例：`// 设置宽度`；正例：`// 撑满宽度，否则卡片会按内容收缩，看起来像个小方块`。
4. **禁止**：
   * 英文注释（专有名词、API 名、日志字段可保留英文）；
   * 注释掉的死代码（直接删，要留痕就交给 git）；
   * 与代码不符的过期注释（改逻辑必须同步改注释）；
   * “TODO”不带归属：格式必须是 `// TODO(#<计划编号>): <具体要做什么>`。
5. **中文标点**：注释正文使用中文标点（，。、：；！？），中英文之间用空格分隔。
6. **界面文案**：所有面向用户的文字使用简体中文，不允许出现英文占位文案。
7. **单元测试**：测试函数名可以用中文（`fn 图标在右下角时面板向右上展开()`），
   但需保证语义清晰。
8. 复杂的平台相关代码（例如 `#[cfg(target_os = "macos")]`）必须写明
   “为什么只有这个平台需要”，并注明未经实机验证的部分。

---

## 五、代码规范

* **格式化**：提交前必须执行 `cargo fmt`；CI/验收使用 `cargo fmt --check`。
* **静态检查**：必须通过 `cargo clippy --all-targets -- -D warnings`（零警告）。
* **错误处理**：
  * 边界处（初始化、IO、外部输入）用 `anyhow::Result` + `.context("…")`；
  * 禁止在业务代码里 `unwrap()` / `expect()`；初始化阶段确实“不可能失败”的地方
    必须写明理由；
  * 互斥锁中毒要显式恢复（参考 `TrayBridge::push`），不要直接 `unwrap`。
* **日志**：
  * `info`：启动、托盘就绪、登录成功等关键节点；
  * `warn`：可恢复的异常（字体缺失、图标解码失败、调试开关启用）；
  * `debug`：事件细节（托盘点击、失焦收起）；
  * **严禁**在日志里打印令牌、Cookie、完整 URL 查询串等凭据信息。
* **命名**：标识符用英文（避免拼音），面向用户的字符串用中文。
* **依赖**：新增依赖需要在本文件“技术栈”表中登记，并说明用途；
  能用标准库解决的不加 crate。
* **定时任务（重要坑）**：eframe 是**按需重绘**的，空闲时不会再调用 `App::logic`。
  任何"过一会儿要做的事"（定时刷新、轮询后台结果、提示自动消失）都必须用
  `ctx.request_repaint_after(duration)` 主动排下一次唤醒，否则永远不会触发。
  参考 `app.rs` 里 `logic()` 末尾的心跳（有刷新在跑 500ms，空闲 5s）。
* **测试分层**：
  * 默认 `cargo test` 必须是**纯逻辑、可重复**的（解析、聚合、状态机、文件读写）；
  * 依赖 `TcpListener` 的端到端用例一律标 `#[ignore]`，验收时用
    `cargo test -- --ignored` 显式运行；
  * 原因：本机安全软件会偶发中止 loopback 连接（`os error 10053`），
    放进默认套件会让"偶发红"掩盖真实回归。公共假服务器代码在
    `main.rs` 的 `test_support` 里（含读完整请求体、串行锁、临时错误重试）。

---

## 六、分支模型与提交流程（必须遵守）

```
main  ← 只由仓库主人在验收后合入（AI 不得提交、不得推送）
 │
 └─ dev  ← 集成分支：每个功能完成后由 AI 合入并推送
      │
      ├─ feat/xxx    新功能
      ├─ fix/xxx     缺陷修复
      ├─ chore/xxx   构建、依赖、脚手架
      └─ docs/xxx    纯文档
```

### 6.1 每个新功能的固定流程

1. **写计划**：在 `docs/plans/` 新建 `NNNN-<英文短名>.md`（复制 `TEMPLATE.md`），
   把“完成的点”“验收标准”“测试方式”“风险”写清楚；
2. **等确认**：计划未经确认前不要写实现代码（用户说“直接做”则视为确认）；
3. **切分支**：从最新 `dev` 切出功能分支，例如
   `git checkout dev && git pull && git checkout -b feat/account-status`；
4. **实现**：小步提交，注释按第四节写；每步保持可编译；
5. **自测**：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、
   `cargo test`、`cargo build`，并按计划文档里的验收标准逐条验证；
6. **合入 dev**：`git checkout dev` → `git merge --no-ff <功能分支>`；
7. **推送**：`git push origin <功能分支>` 与 `git push origin dev`，
   然后告知用户“功能分支与 dev 已更新到远端，等待你合入 main”。

### 6.2 硬性纪律

* **禁止**直接向 `main` 提交或推送；`main` 只接受用户在验收后合入；
* **禁止**在 `dev` 上直接写业务代码（仅允许 `--no-ff` 合入功能分支，
  以及极小的hotfix，且同样需要先开分支）；
* 一个分支只做一件事；分支生命周期尽量短；
* 合入前必须先把 `dev` 的更新合进功能分支（`git merge dev`），避免最后一次性冲突；
* 推送失败、权限不足时**不要**改用强制推送（`--force`），先报告问题。

### 6.3 约定式提交（Conventional Commits）+ 全中文

格式：

```
<类型>(<范围>): <中文描述>

<可选正文：为什么这么改、影响面、验证方式>

<可选脚注：Refs: docs/plans/0002-xxx.md>
```

* **类型**只用：`feat` `fix` `docs` `chore` `refactor` `test` `perf` `build` `ci` `style` `revert`；
* **范围**（scope）用模块名：`tray`、`app`、`fonts`、`deps`、`docs`、`build` 等；
* **描述一律简体中文**，不加句号，不超过 50 字；
* 正文同样使用中文，说明“为什么”和“验证方式”；
* 一个提交只包含一个逻辑变更；格式化与逻辑修改不要混在同一个提交里。

示例：

```
feat(tray): 支持左键单击切换面板显示

托盘单击原先只弹出菜单，用户仍要再点一次“显示面板”。
现改为左键直接切换面板、右键保留菜单，符合 Windows 托盘工具习惯。

验证：cargo test；手动点击托盘图标确认面板开合并自动失焦收起。
Refs: docs/plans/0002-tray-click-toggle.md
```

---

## 七、功能计划与验收标准

每个功能都必须在 `docs/plans/NNNN-<slug>.md` 里先写计划，字段固定：

| 字段 | 要求 |
| --- | --- |
| 状态 | 计划中 / 进行中 / 已完成 / 已合入 dev |
| 背景 | 为什么要做，现状是什么 |
| 目标 | 一句话说明完成后用户能做什么 |
| 范围 | **包含什么**，以及**明确不包含什么** |
| 完成点 | 可勾选的实现清单，一条一事 |
| 验收标准 | 可执行、可观测的判定条件（见下） |
| 测试方式 | 自动化测试 + 手工验证步骤（含平台） |
| 风险与回滚 | 可能出问题的地方与回退办法 |
| 涉及文件 | 预计改动的文件列表 |
| 结果 | 完成后填写：验证记录、遗留问题 |

**验收标准必须可验证**，反例与正例：

* ❌ 反例：“面板显示正常。”
* ✅ 正例：“点击托盘图标后 200ms 内面板出现在图标正上方，右边缘与图标右边缘对齐，
  且不超出屏幕可用区域；再次点击或点击面板外部时收起。”

功能完成后：勾选完成点、补写“结果”段落、把状态改成“已完成”，
随功能分支一起提交。

---

## 八、提交前检查清单

- [ ] `cargo fmt --check` 通过
- [ ] `cargo clippy --all-targets -- -D warnings` 无警告
- [ ] `cargo test` 全部通过
- [ ] `cargo build` 通过
- [ ] 计划文档里的验收标准逐条验证完毕（记录在计划文档“结果”里）
- [ ] 新增/修改的代码都有简体中文注释，且注释与实现一致
- [ ] 提交信息符合第六节的约定式提交格式（中文）
- [ ] 没有把 `target/`、日志、本地截图等临时文件加入版本库

---

## 九、常用命令

```powershell
# Windows（PowerShell）
cargo run                              # 调试运行（托盘程序，启动后面板默认隐藏）
$env:SUB2API_TRAY_DEBUG_VISIBLE = "1"; cargo run   # 启动即显示面板，便于截图验收
$env:RUST_LOG = "debug"; cargo run     # 打开调试日志
cargo fmt                              # 格式化
cargo fmt --check                      # 只检查
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release                  # 产物：target\release\sub2api_tray.exe
```

```bash
# macOS（zsh/bash）
cargo run
SUB2API_TRAY_DEBUG_VISIBLE=1 cargo run
RUST_LOG=debug cargo run
cargo build --release                  # 产物：target/release/sub2api_tray
```

调试开关说明：

* `SUB2API_TRAY_DEBUG_VISIBLE`：存在即生效，启动后直接弹出面板，并且**不因失焦自动收起**；
  原因是托盘程序平时“隐身”，而自动化截图时窗口通常拿不到焦点，否则面板会在截图前消失。
* `SUB2API_TRAY_DEBUG_LOGIN`：存在即生效，启动后自动弹出面板并打开登录弹窗；
  用于截图验收弹窗布局（自动化没法真的去点“登陆新用户”按钮）。
* `SUB2API_TRAY_DEBUG_EXPAND`：存在即生效，启动后自动展开第一个账号的 Key 明细，
  用于自动化截图验收展开态与悬浮高亮。
* `RUST_LOG`：`debug` 可看到托盘点击、失焦收起等细节。

---

## 十、平台差异备忘

| 事项 | Windows | macOS |
| --- | --- | --- |
| 托盘位置 | 任务栏通知区域，一般贴屏幕右下 | 菜单栏，贴屏幕右上 |
| 图标矩形 `TrayIcon::rect()` | 可靠返回 | 通常 `None`，需走兜底定位 |
| 透明窗口 | 需要 glow（OpenGL）；wgpu 不支持 alpha | 两种后端都可用 |
| 面板出现位置 | 图标上方（避免被任务栏切掉） | 图标下方 |
| Dock / 任务栏图标 | `with_taskbar(false)` 已隐藏面板窗口 | 后续需要 `ActivationPolicy::Accessory` 隐藏 Dock 图标（未实现） |
| 中文字体 | `C:\Windows\Fonts\msyh.ttc` | `/System/Library/Fonts/PingFang.ttc` |
| 托盘图标模板色 | 彩色图标 | 建议使用 template 图标（单色），当前占位图未适配 |
| 开机自启 / 单实例 | 未实现 | 未实现 |

> macOS 相关内容**尚未在实机验证**。修改 macOS 分支代码时，必须在计划文档里
> 写明“需要在 macOS 上由人工验收”的条目，不要以“逻辑上应该没问题”结案。

---

## 十一、安全与隐私

* 托盘工具会接触 Sub2API 的登录凭据，**任何令牌都不允许**写入日志、明文配置文件或
  提交进版本库；
* 后续实现登录态持久化时：Windows 优先使用 DPAPI（`CryptProtectData`），
  macOS 使用钥匙串（Keychain），不要自己发明加密方案；
* 用户可见的账户信息只在面板内展示，不落盘、不上传到第三方；
* 引入任何会外发数据的依赖前，必须在计划文档里说明并取得确认。

---

## 十二、数据文件与配置

`app.json` 与 `accounts.json` 都放在**可执行文件所在目录**（便携式）；目录不可写时
程序直接报错、不创建文件、不静默换目录（0002 的验收标准之一）。

### `app.json`

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `schemaVersion` | number | `1` | 文件版本，将来迁移用 |
| `maskAccounts` | bool | `false` | 账号名是否打码展示（只影响 UI） |
| `refreshIntervalSeconds` | number | `300` | 状态刷新间隔，合法范围 30～86400 |
| `requestTimeoutSeconds` | number | `10` | 单次 HTTP 超时，合法范围 1～120 |
| `panelPinned` | bool | `false` | 面板是否钉住（置顶显示，点外部不收起） |

* 缺失字段用默认值补齐；非法值回落到默认值并写 `warn` 日志，同时**回写**文件；
* 语法损坏时改名为 `app.json.corrupt-<时间戳>` 后用默认值重建；
* **改完需要重启**（不做热加载）。

### `accounts.json`

| 字段 | 说明 |
| --- | --- |
| `schemaVersion` | 文件版本 |
| `accounts[].id` | UUID v4，稳定标识 |
| `accounts[].baseUrl` | 实例地址，已归一化（无尾斜杠、无查询参数） |
| `accounts[].email` | 登录邮箱，原样保存（展示时才打码） |
| `accounts[].role` | `admin` / `user` / `unknown` |
| `accounts[].secrets` | 不透明加密块，格式由 `secrets.rs` 决定 |
| `accounts[].createdAt` | RFC3339 UTC，**决定展示顺序（升序）** |
| `accounts[].lastLoginAt` / `tokenExpiresAt` | 可选，RFC3339 |
| `accounts[].expanded` | 面板里是否保持展开（UI 状态，跨重启保留，默认 `false`） |
| `accounts[].apiKeys` | API Key 摘要缓存（只存打码值与元信息，不含完整 Key） |

不变量：`baseUrl + email` 唯一（忽略大小写与尾斜杠）；排序由程序维护，不要手改 `createdAt`；
语法损坏时同样改名备份并重建为空列表。

### 凭据

| 平台 | `secrets` 形态 | 解密条件 |
| --- | --- | --- |
| Windows | `enc:dpapi:v1:<base64>` | 同机 + 同 Windows 用户 |
| macOS | `enc:keychain:v1:<账号 id>` | 同 macOS 用户（首次弹钥匙串授权） |

明文只包含 `{ password, accessToken, refreshToken }`，解密失败一律视为“凭据损坏、需重新登录”。

---

## 十三、变更记录

| 日期 | 变更 | 说明 |
| --- | --- | --- |
| 2026-10-18 | 初版 | 建立工程定位、注释规范、分支流程、功能计划与验收标准 |
| 2026-10-18 | 补充数据层约定 | 新增第七节技术栈条目（serde/url/uuid/time/base64/DPAPI/钥匙串）、第十二节数据文件与配置；阶段表拆分为 1a/1b/1c；调试开关补充“不自动收起” |
| 2026-10-18 | 补充登录约定 | 技术栈登记 `ureq`；目录结构补 `api.rs`/`login.rs`；新增 `SUB2API_TRAY_DEBUG_LOGIN` 调试开关；阶段 1b 标记完成 |
| 2026-10-18 | 补充测试分层约定 | 默认套件必须可重复；loopback 端到端用例标 `#[ignore]`，验收时显式运行；假服务器公共代码集中在 `main.rs::test_support`；阶段 2 标记完成 |
| 2026-10-18 | 补充 API Key 约定 | 阶段表新增 1d；`accounts.json` 的 `apiKeys` 只存摘要（id/名称/打码值/状态/分组/最近使用/抓取时间），**不存完整 Key** |
| 2026-10-18 | 补充浏览约定 | 阶段表新增 1e；明确"托盘"= 弹出面板（原生菜单不能滚动/展开）；账号上限 10；面板默认不显示数据目录 |
| 2026-10-18 | 账号用量曲线 | 展开区新增「用量」：24 小时/7 天/30 天切换，走用户态接口 `/usage/dashboard/trend`（稀疏点），用 `Painter` 原生绘制折线 + 悬浮提示，缓存 60 秒 |
| 2026-10-18 | 发布体积瘦身 | release 体积 13.3MB → 7.6MB：`image` 换成只解 PNG 的 `png`（-5.7MB 的主要来源）、`opt-level="z"`、`panic="abort"`、保留 `lto + strip + codegen-units=1` |
| 2026-10-18 | 面板置顶（钉住） | 右上角新增图钉按钮（矢量绘制）：钉住后点面板外部不再自动收起，状态持久化到 `app.json` 的 `panelPinned` |
| 2026-10-18 | 面板可缩放 + 登录入口内嵌 | 面板改为可缩放（最小 320×240），定位改用窗口真实尺寸；右下角自绘缩放手柄（内缩 8px 避开系统命中区，否则系统会抢走拖拽）；「登陆新用户」并入顶部总览卡片做高亮按钮 |
| 2026-10-18 | 删除冗余的 Key 弹窗 | 展开区已经展示名称/打码值/额度/用量/分组，点击即复制，弹窗只剩重复内容，整块删除（约 300 行）；退出按钮移到账号标题行右侧 |
| 2026-10-18 | 菜单只留原子动作 | 右键菜单最终只剩「立即刷新 / 版本 / 退出」：面板靠左键切换、登录在面板顶部，所以去掉「显示面板」「登陆新用户」 |
| 2026-10-18 | 账号入口收拢到面板 | 原生右键菜单不再列账号（系统菜单不能滚动/展开）；查看 Key、退出账号改为面板内按钮，删除仍走二次确认 |
| 2026-10-18 | 新增展开状态持久化 | `accounts[].expanded` 记录面板展开状态（UI 状态放账号记录里，删账号即清）；启动时恢复并依次拉取 Key |
| 2026-10-18 | 新增发布流水线 | `.github/workflows/release-windows.yml`：`v*` tag 触发，校验 tag 在 main 历史、跑质量门禁、编 Windows x64 并挂 Release；只支持 Windows 10+ |
| 2026-10-18 | 补充自启与更新 | 开机自启用 HKCU Run / LaunchAgent，命令行带 --hidden；版本检查走 GitHub Releases，只提示不自动安装 |
| 2026-10-18 | 补充单实例与设置 | 单实例锁用固定本机端口（**不能用端口段**，否则第二个实例会顺延端口绑上）；设置页可改打码/刷新间隔/超时并即时落盘 |
| 2026-10-18 | 补充账号菜单约定 | 目录结构补 `state.rs`/`refresh.rs`；代码规范新增“eframe 按需重绘，定时任务必须 request_repaint_after”这一坑点；阶段 1c 标记完成 |
