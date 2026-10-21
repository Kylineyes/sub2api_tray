# 0025 托盘标题更名与禁用文本拖选

| 字段 | 内容 |
| --- | --- |
| 编号 | 0025 |
| 分支 | `feat/api-key-quota-adjust` |
| 状态 | 已完成 |
| 提出日期 | 2026-10-22 |

## 一、背景

顶部标题仍为「Sub2API 托盘」，与当前产品定位不一致；同时 egui 默认允许拖选 Label 文本，
拖动标题或滑动面板时容易出现蓝色文字选区，影响托盘面板的操作感。

## 二、目标

将用户可见标题改为「用量统计托盘」，并禁止鼠标拖选普通界面文字。

## 三、范围

**包含：**

* 标题栏、托盘提示、单实例提示、README、Cargo 描述统一使用「用量统计托盘」；
* 全局关闭 egui Label 的文本选择与跨 Label 选择。

**不包含：**

* 不修改输入框中的文本编辑能力；
* 不修改 Sub2API 实例、登录等业务文案。

## 四、完成点

* [x] 顶部标题改为「用量统计托盘」；
* [x] 托盘与单实例提示同步改名；
* [x] 全局设置 `selectable_labels = false`；
* [x] 同步 `multi_widget_text_select = false`。

## 五、验收标准

1. 操作：打开托盘面板 → 预期：顶部显示「用量统计托盘」。
2. 操作：鼠标按住并拖过普通文字 → 预期：不出现文字选区。
3. 操作：点击输入框编辑 → 预期：输入框内仍可正常选择和编辑文本。

## 六、测试方式

* `cargo fmt --check`
* `cargo clippy --all-targets -- -D warnings`
* `cargo test`
* `cargo build --release`

## 七、风险与回滚

| 风险 | 影响 | 应对 |
| --- | --- | --- |
| 全局关闭文字选择影响复制 | 用户无法拖选 UI 文本 | Key 复制仍走显式按钮，不影响业务 |

回滚方式：恢复 `APP_NAME` 与 `apply_egui_visuals` 中两项选择设置。

## 八、涉及文件

* `src/app.rs`
* `src/main.rs`
* `src/tray.rs`
* `src/single_instance.rs`
* `README.md`
* `Cargo.toml`
* `docs/plans/0025-app-title-no-selection.md`

## 九、结果

* 完成日期：2026-10-22
* 实际验证记录：`cargo fmt`、`cargo clippy --all-targets -- -D warnings`、`cargo test`
  与 release 构建通过。
* 与计划的偏差：无。
* 遗留问题：无。
