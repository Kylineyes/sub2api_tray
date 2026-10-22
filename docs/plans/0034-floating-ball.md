# 0034 桌面悬浮球

| 字段 | 内容 |
| --- | --- |
| 编号 | 0034 |
| 分支 | `feat/floating-ball` |
| 状态 | 已完成 |
| 提出日期 | 2026-10-23 |

## 一、目标

右键托盘菜单可以打开桌面悬浮球，并选择主账号或主 API Key：

- 悬浮球常驻桌面并置顶；
- 支持贴边自动收起（可配置）；
- 左键点击悬浮球打开主面板；
- 默认在正中心展示账号余额或主 Key 剩余额度；
- 悬浮球不显示刷新倒计时，后台自动刷新。

## 二、实现

* `app.json` 新增 `floatingBallVisible`、`floatingBallEdgeHide`、`floatingBallAccountId`、`floatingBallApiKeyId`；
* 托盘菜单新增「悬浮球」子菜单：显示/隐藏、贴边收起开关、主账号/主 Key 选择；
* 主账号/主 Key 使用原生 `CheckMenuItem` 单选联动，并提供「刷新 Key 列表」入口；
* 使用 egui immediate viewport 创建无边框、透明、置顶、任务栏隐藏的 64×64 悬浮球；
* 未悬浮时绘制为右侧窄条，鼠标悬停展开为圆形；该行为受 `floatingBallEdgeHide` 控制；
* 左键点击悬浮球调用 `show_panel`；
* 配色使用 `card_bg` / `hover_card_bg`、`accent` 描边、`text_primary` 数值和状态点，不使用整块高饱和品牌色；
* 账号余额由现有刷新链更新；主 Key 每 60 秒拉一次 Key 列表并更新剩余额度；
* 悬浮球只展示最终数值，不展示刷新倒计时。

## 三、边界

* DeepSeek 官方账号只展示人民币余额；
* Sub2API 账号可展示余额，主 Key 展示剩余额度（不限额显示「不限」）；
* 未选择目标时显示「设置」。

## 四、验证

* `cargo fmt`
* `cargo clippy --all-targets -- -D warnings`
* `cargo test`：130 passed，8 ignored
* `cargo build --release`
* 实机验证悬浮球窗口创建、余额展示和贴边收起绘制；
* 悬浮球直接在透明 viewport 外层 Ui 绘制，不套 CentralPanel，避免贴边收起时出现半透明方形底层。

## 五、结果

* 完成日期：2026-10-23
* 主面板和悬浮球共用同一份刷新结果与配置持久化。
