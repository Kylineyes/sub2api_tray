# 0035 DeepSeek 奖励金额

| 字段 | 内容 |
| --- | --- |
| 编号 | 0035 |
| 分支 | `feat/deepseek-reward-balance` |
| 状态 | 已完成 |
| 提出日期 | 2026-10-23 |

## 一、目标

DeepSeek 官方余额接口除了总余额，还会返回：

- `granted_balance`：赠送/奖励余额；
- `topped_up_balance`：充值余额。

面板与悬浮球需要展示奖励金额，不能只显示总额。

## 二、实现

* `deepseek::DeepSeekBalance` 新增 `granted_balance`、`topped_up_balance`；
* 解析失败或字段缺失时按 `0.00` 容错；
* `MeInfo`、`AccountRuntime`、`AccountCard` 增加对应字段；
* DeepSeek 账号展开态显示：
  `在线 · 余额 ¥88.88 · 奖励 ¥12.50 · 充值 ¥76.38`；
* 悬浮球选中 DeepSeek 账号时，中心显示总余额，下方小字显示 `奖励 ¥12.50`；
* 演示夹具同步补充奖励/充值字段。

## 三、验证

* `cargo fmt`
* `cargo clippy --all-targets -- -D warnings`
* `cargo test`：130 passed，8 ignored
* `cargo build --release`

## 四、结果

* 完成日期：2026-10-23
* Sub2API 账号行为不变。
