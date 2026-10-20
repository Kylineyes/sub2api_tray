# 0015 Sub2API 双主题（日间/夜间）

| 字段 | 内容 |
| --- | --- |
| 编号 | 0015 |
| 分支 | feat/light-dark-theme |
| 状态 | 已完成 |
| 提出日期 | 2026-10-19 |
| 依赖 | 0014（面板拖动与缩放） |

## 一、背景

面板目前只有固定的深色配色，用户无法在白天使用浅色外观。Sub2API 前端已经有明确的品牌色板：
主色 Teal/Cyan（primary-500 `#14b8a6`），浅色底 `gray-50`，深色底 `dark-950 #020617`。
托盘需要按同一套色板提供日间和夜间两种主题，并记住用户选择。

## 二、目标

用户可以在标题栏一键切换日间/夜间主题，也可以在设置中保存主题偏好；切换后整个面板、弹窗和输入控件同步换色。

## 三、范围

包含：

* 新增日间与夜间两套完整 Palette；
* 面板背景、卡片、边框、文字、强调色、错误色、图表颜色全部主题化；
* 标题栏日/月切换按钮；
* 设置页主题选项，保存到 `app.json`；
* egui 默认控件样式跟着主题同步；
* 启动时恢复上次主题。

不包含：

* 不跟随操作系统主题自动切换；
* 不新增多套品牌配色；
* 不改变托盘图标本身。

## 四、完成点

* [x] `app.json` 增加 `theme` 字段并做默认/非法值修复
* [x] 日间/夜间 Palette 与 egui Visuals 同步
* [x] 替换所有固定颜色为主题色
* [x] 标题栏切换按钮与设置页选项
* [x] 单测与实机截图

## 五、验收标准

1. 点击标题栏日/月按钮 -> 面板立即在浅色和深色之间切换。
2. 打开设置 -> 主题选项与当前主题一致，保存后重启仍保持。
3. 日间模式下背景、卡片、文字、边框、图表、弹窗和输入控件均可读。
4. 夜间模式保持现有深色观感，同时使用 Sub2API Teal 强调色。
5. 旧版 `app.json` 没有 `theme` 字段时默认夜间，不损坏配置。

## 六、测试方式

* cargo fmt --check
* cargo clippy --all-targets -- -D warnings
* cargo test --locked
* cargo build --release --locked
* Windows 实机切换日夜主题并检查弹窗与用量区

## 七、风险与回滚

| 风险 | 影响 | 应对 |
| --- | --- | --- |
| 遗漏硬编码颜色 | 浅色模式出现深色块 | 统一通过 Palette 访问，代码搜索确认无固定主题色残留 |
| egui 弹窗仍用旧样式 | ComboBox/输入框不协调 | 每帧根据主题重建 all_styles 的 Visuals |
| 旧配置字段缺失 | 启动失败 | serde default + sanitize 回落到夜间 |

回滚方式：git revert 合并提交。

## 八、涉及文件

* src/config.rs：theme 字段与默认值
* src/app.rs：Palette、主题状态、切换按钮、设置项
* src/login.rs：弹窗颜色改为主题化
* README.md / AGENTS.md
* docs/plans/0015-light-dark-theme.md

## 九、结果

* 完成日期：2026-10-20
* 实际验证记录：
  * `cargo fmt --check` 通过；
  * `cargo clippy --all-targets -- -D warnings` 通过；
  * `cargo test --locked`：104 passed / 6 ignored / 0 failed；
  * `cargo build --release --locked` 通过；
  * 夜间与日间两套截图均已生成，背景、卡片、文字、图表和弹窗颜色与 Sub2API 品牌色一致。
* 与计划的偏差：标题栏入口最初右对齐，在窄窗口下会被挤出可视区，最终改为标题右侧左对齐排列。
* 遗留问题 / 后续计划编号：主题暂不跟随操作系统自动切换。
