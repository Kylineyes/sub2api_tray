# 界面样式规范

本文件是 `AGENTS.md` 的 UI 子规范。所有界面开发必须先读这里，再改 `src/app.rs`、`src/login.rs` 等绘制代码。

## 一、唯一来源

所有颜色、字号、按钮尺寸、圆角和边距统一来自：

```text
src/ui_style.rs
```

绘制代码不允许再新增以下硬编码：

- `egui::Color32::from_rgb(...)` / `from_rgba(...)`
- `.size(12.0)` 这类字号
- `egui::Button::new(...)` 直接构造按钮
- `add_sized([92.0, 24.0], ...)` 直接写按钮尺寸
- `CornerRadius::same(12)` / `Margin::same(16)` 这类框架尺寸

需要新样式时，先在 `src/ui_style.rs` 增加语义化 token，再在界面代码引用。

## 二、颜色

主题色板定义在 `ui_style.rs` 的 `Palette`：

| Token | 用途 |
| --- | --- |
| `panel_bg` | 面板/弹窗背景 |
| `panel_border` | 面板、卡片、弹窗边框 |
| `card_bg` / `hover_card_bg` | 卡片默认/悬浮背景 |
| `empty_bg` | 空状态、输入控件背景 |
| `text_primary` | 标题与主要文字 |
| `text_secondary` | 正文、说明文字 |
| `text_dim` | 弱化提示、辅助信息 |
| `accent` / `on_accent` | 品牌色与品牌色上的文字 |
| `danger*` | 错误、删除、危险操作 |
| `info` / `offline` | 刷新中/离线状态 |
| `grid_line` / `overlay_subtle` | 分割线、极淡悬浮层 |
| `tooltip_bg` / `input_bg` | 图表提示与输入框 |

规则：

1. 业务代码只使用 `theme().token`，不要直接写颜色值。
2. 新增状态色必须先加入两套 Palette（日间/夜间），不能只改一个主题。
3. 透明色只允许用于动画起点等语义明确的场景。

## 三、字号

字号使用 `ui_style::font`：

| Token | 用途 |
| --- | --- |
| `CAPTION` | 极小辅助文字 |
| `SMALL` | 弱化说明 |
| `SMALL_PLUS` | 次要正文 |
| `BODY_SMALL` | 常规小正文、按钮 |
| `BODY_SMALL_PLUS` | Key 操作按钮、强调小正文 |
| `BODY` | 正文 |
| `BODY_LARGE` | 表单按钮、强调正文 |
| `SUBHEADING` | 小节标题 |
| `HEADING` | 卡片标题 |
| `TITLE` | 弹窗标题 |

用量区使用 `ui_style::usage_font`，不要直接放大字号。

## 四、按钮

按钮统一使用：

```rust
ui_style::add_button(ui, "保存", ButtonTone::Primary, ButtonSize::Form);
ui_style::add_enabled_button(ui, enabled, "删除", ButtonTone::Danger, ButtonSize::Auto);
```

### 语义色

| `ButtonTone` | 用途 |
| --- | --- |
| `Neutral` | 普通操作 |
| `Primary` | 确认、创建、登录等主操作 |
| `Danger` | 删除、退出、危险操作 |
| `Ghost` | 选中态切换、透明轻量按钮 |

### 尺寸档位

| `ButtonSize` | 尺寸 | 用途 |
| --- | --- | --- |
| `Tiny` | 64×22 | 登录入口等紧凑按钮 |
| `Xs` | 76×22 | 总览卡片小按钮 |
| `KeyAction` | 64×26 | Key 悬浮操作 |
| `Header` | 104×24 | 账号标题行按钮 |
| `Form` | 80×26 | 表单底部按钮 |
| `FormMd` | 100×26 | 表单宽按钮 |
| `Wide` | 120×22 | 链接类宽按钮 |
| `Sm` / `Md` / `Lg` | 标准档位 | 弹窗操作 |
| `Auto` | 宽度随内容 | 次级按钮 |

禁止在绘制代码里直接 `Button::new` 或 `add_sized` 设置按钮尺寸。

## 五、卡片、弹窗与间距

- 卡片圆角：`ui_style::size::RADIUS_LG`
- 弹窗圆角：`ui_style::size::RADIUS_XL`
- 弹窗内边距：`ui_style::size::MODAL_MARGIN`
- 卡片/提示条内边距：`MARGIN_SM` / `MARGIN_MD`
- 弹窗优先使用 `ui_style::modal_frame()`
- 卡片优先使用 `ui_style::card_frame()`

## 六、表单控件

- 字段标签使用 `ui_style::field_label`；
- 普通输入框使用 `ui_style::singleline_input`；
- 货币输入使用 `ui_style::currency_input`，由 `$` 前缀和金额输入框组成同一行、同一高度；
- 分组等下拉框的文字使用 `font::SUBHEADING`，避免与输入框正文视觉差距过大；
- 日期选择使用 `ui_style::date_picker`，小时/分钟使用 `ui_style::number_input`；
- 说明文字使用 `ui_style::hint_label`。

## 七、日期与时间

- 日期选择统一使用 `egui_extras::DatePickerButton`，关闭日历网格、箭头和图标，只保留年/月/日下拉，避免展示星期信息；
- 小时/分钟使用 `egui::DragValue`，范围分别为 `0..=23` 和 `0..=59`；
- 用户选择的是本地时间，提交前通过 `jiff::tz::TimeZone::system()` 转成 RFC3339；
- 不要再让用户手填 RFC3339 字符串。

## 八、设置页

* 使用 `ui_style::modal_frame` 和 `ui_style::section_frame`；
* 分组标题使用 `ui_style::section_title`；
* 字段标签使用 `ui_style::field_label`；
* 数字输入使用 `ui_style::drag_number`，宽度为 `size::NUMBER_INPUT_WIDTH`；
* 说明文字使用 `ui_style::hint_label`；
* 主操作「保存」使用 `ButtonTone::Primary`，次操作「取消」使用 `ButtonTone::Neutral`。

## 九、悬浮球

- 尺寸 token：`size::FLOATING_BALL_SIZE`
- 收起宽度：`size::FLOATING_BALL_COLLAPSED_WIDTH`
- 屏幕边距：`size::FLOATING_BALL_MARGIN`
- 球形背景：`theme().card_bg`，悬浮时 `theme().hover_card_bg`
- 外圈：`theme().accent` 描边，内圈可使用 `theme().chart_fill`
- 数值文字：`theme().text_primary`
- 状态点：`status_color`
- 不使用整块高饱和品牌色填充

## 十、异步操作

* 写磁盘、注册表、网络请求等可能阻塞的操作不得放在 UI 线程里同步执行；
* 操作进行中必须展示明确的阻塞式进度弹窗或行内 loading；
* 进度期间禁用重复提交，不允许 `Esc` 或点击遮罩取消；
* 设置保存使用「正在保存设置」弹窗，后台线程完成后才关闭设置页。

## 十一、交互约定

1. Key 的「复制」「调额度」「删除」只在悬浮该 Key 时出现；未悬浮时不保留按钮行高度，保持列表紧凑。
2. 账号标题行只有折叠箭头和邮箱可触发展开/收起；右侧按钮必须有独立点击热区。
3. 危险操作必须有二次确认。
4. 所有面向用户的文案使用简体中文。
5. 不允许在 Label 上拖选文本。

## 十二、开发检查

提交前至少执行：

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

并自查：

- 是否新增了硬编码颜色/字号/按钮尺寸；
- 是否新增了未登记的颜色或尺寸 token；
- 日间/夜间主题是否都能正常显示；
- 悬浮、禁用、危险操作是否有清晰反馈。
