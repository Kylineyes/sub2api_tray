# 0032 DeepSeek 官方账号支持

| 字段 | 内容 |
| --- | --- |
| 编号 | 0032 |
| 分支 | `feat/deepseek-platform` |
| 状态 | 已完成 |
| 提出日期 | 2026-10-23 |

## 一、目标

新增账号时可以选择来源：

- Sub2API 中转站（原有邮箱密码登录）
- DeepSeek 官方（`platform.deepseek.com`，填写 API Key）

DeepSeek 官方账号只读取余额，不展示 Sub2API 的 Key 列表和用量曲线。

## 二、实现

* `accounts.rs` 新增 `AccountProvider`，旧 `accounts.json` 缺失字段时默认 `sub2Api`；
* 登录弹窗新增来源下拉；DeepSeek 模式只显示 API Key 输入框；
* API Key 通过 `https://api.deepseek.com/user/balance` 校验，并读取 CNY 余额；
* API Key 继续用 DPAPI/钥匙串加密保存在 `accounts.json` 的 `secrets` 字段；
* 刷新流程按 provider 分派，DeepSeek 不走 Sub2API 的续期/重登链；
* 面板对 DeepSeek 账号隐藏「新增key / 我的用量 / Key 明细」，只显示在线状态与余额。

## 三、兼容性

旧 `accounts.json` 没有 `provider` 字段时按 `sub2Api` 读取，不影响已有账号。

DeepSeek API Key 绑定当前 Windows 用户/机器，不能跨用户直接复制密文使用。

## 四、验证

* 实测测试 API Key 返回余额 `¥19.65`，面板显示在线与该余额；
* `cargo fmt`
* `cargo clippy --all-targets -- -D warnings`
* `cargo test`：128 passed，8 ignored
* `cargo build --release`

## 五、安全

测试 API Key 未写入源码、测试或提交，只用于本地临时验证，验证目录已删除。
