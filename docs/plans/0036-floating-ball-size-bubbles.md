# 0036 悬浮球尺寸与更新气泡

| 字段 | 内容 |
| --- | --- |
| 编号 | 0036 |
| 分支 | `feat/floating-ball-size-bubbles` |
| 状态 | 已完成 |
| 提出日期 | 2026-10-23 |

## 一、目标

- 设置页支持悬浮球尺寸：小 / 中 / 大，默认中；
- 悬浮球数据刷新并发生变化时，播放鱼吐泡泡式动效：
  3～4 个半透明气泡从球体底部曲线上升，到顶部淡出。

## 二、实现

* `app.json` 新增 `floatingBallSize`，默认 `medium`；
* 设置页「外观」增加尺寸三档按钮；
* 尺寸 token：
  - `FLOATING_BALL_SIZE_SMALL = 52`
  - `FLOATING_BALL_SIZE_MEDIUM = 64`
  - `FLOATING_BALL_SIZE_LARGE = 80`
* 刷新前后比较悬浮球展示快照，发生变化时记录动画起点；
* 动画持续约 1.5 秒，4 个气泡错峰上升，使用正弦曲线横向摆动并逐渐透明；
* 动画期间主动 `request_repaint_after(16ms)`，避免空闲不重绘。

## 三、验证

* `cargo fmt`
* `cargo clippy --all-targets -- -D warnings`
* `cargo test`：134 passed，8 ignored
* `cargo build --release`
* 实机验证尺寸切换写盘、窗口尺寸变化和气泡动画。

## 四、结果

* 完成日期：2026-10-23
* 气泡只在展示数据实际变化时触发，普通心跳刷新不会反复冒泡。
