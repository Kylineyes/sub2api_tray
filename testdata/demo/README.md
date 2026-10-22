# 演示账号与展示夹具

本目录是托盘 UI 截图、展示验收和手工调试的唯一演示数据来源。

后续需要更多展示数据时，直接在本目录继续补充，不要另造一套演示账号。

## 固定账号

### Sub2API 中转站

| 字段 | 值 |
| --- | --- |
| 邮箱 | `demo@example.com` |
| 密码 | `demo-password`（仅假服务器使用） |
| 实例地址 | `http://127.0.0.1:18080` |
| 角色 | `user` |
| 余额 | `$42.50` |

### DeepSeek 官方

| 字段 | 值 |
| --- | --- |
| 展示用 Key | `sk-...demo` |
| 币种 | `CNY` |
| 余额 | `¥88.88` |
| provider | `deepSeekPlatform` |

## 固定数据

| 文件 | 内容 |
| --- | --- |
| `accounts.template.json` | Sub2API 演示账号与两个 Key 的本地缓存，`__SECRET__` 由生成脚本替换 |
| `deepseek_account.template.json` | DeepSeek 官方演示账号模板 |
| `app.json` | 演示用深色主题配置 |
| `api/me.json` | `/auth/me` 的 `data` |
| `api/subscriptions.json` | 订阅摘要 |
| `api/keys.json` | Key 列表 |
| `api/usage.json` | 用量曲线 |
| `api/groups.json` | 可用分组 |
| `api/deepseek_balance.json` | DeepSeek 余额原始响应 |

固定 Key：

- `Demo Key A`：额度 `$100.00`，已用 `$12.35`，5 小时限速 `$1.25 / $50.00`，1 天限速 `$4.50 / $200.00`
- `Demo Key B`：不限额，已用 `$3.21`

固定用量汇总：

- 总 Token `446.0k`
- 输入 `219.0k`
- 输出 `93.0k`
- 命中率 `29.7%`
- 请求 `167`
- 花费 `$0.850`

## 生成可运行演示目录

```powershell
powershell -ExecutionPolicy Bypass -File scripts/demo/make_demo.ps1
```

默认生成到 `target/demo-fixture`，并复制当前 release exe。

启动假 Sub2API：

```powershell
python scripts/demo/fake_server.py
```

然后运行：

```text
target/demo-fixture/sub2api_tray.exe
```

## 规则

- 禁止把真实 API Key、真实邮箱、真实实例地址写入本目录；
- `__SECRET__` 只能由本地脚本在 `target` 下替换，不能提交真实 DPAPI 密文；
- 新增展示数据优先追加到现有 JSON，而不是新建第二套演示账号；
- Rust 单元测试 `src/test_fixtures.rs` 会检查本目录关键字段，修改后必须保持通过。
