# 0031 兼容字符串业务码

| 字段 | 内容 |
| --- | --- |
| 编号 | 0031 |
| 分支 | `fix/string-api-code` |
| 状态 | 已完成 |
| 提出日期 | 2026-10-22 |

## 一、问题

Sub2API 后端把响应信封的 `code` 从数字改成了字符串：

```json
{"code":"TOKEN_EXPIRED","message":"Token has expired"}
```

托盘按 `code: i64` 解析，字符串导致整个信封反序列化失败，被误报为「不是有效的 Sub2API 实例」，token 续期链无法启动。

## 二、修复

* `Envelope.code` 改为 `serde_json::Value`，同时接受数字与字符串；
* 字符串业务码映射到 HTTP 语义：
  * `UNAUTHORIZED` / `UNAUTHENTICATED` / `TOKEN_EXPIRED` / `TOKEN_INVALID` / `INVALID_TOKEN` / `AUTH_REQUIRED` -> 401
  * `FORBIDDEN` / `INSUFFICIENT_PERMISSIONS` -> 403
  * `NOT_FOUND` / `RESOURCE_NOT_FOUND` -> 404
  * `RATE_LIMITED` / `RATE_LIMIT_EXCEEDED` / `TOO_MANY_REQUESTS` -> 429
  * 未知非空字符串 -> 500
* 增加字符串 `TOKEN_EXPIRED` 的单元测试。

## 三、验证

* 实测旧账号刷新后恢复在线并更新余额；
* `cargo fmt`
* `cargo clippy --all-targets -- -D warnings`
* `cargo test`：125 passed，8 ignored
* `cargo build --release`
