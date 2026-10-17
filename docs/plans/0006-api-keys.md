# 0006 API Key 查看与复制（accounts.json 内的 Key 缓存）

| 字段 | 内容 |
| --- | --- |
| 编号 | 0006 |
| 分支 | `feat/api-keys` |
| 状态 | 计划中（等待范围确认） |
| 提出日期 | 2026-10-18 |
| 依赖 | 0002（`apiKeys` 字段预留）、0004（账号运行态与面板卡片） |

## 一、背景

需求方最初就要求：`accounts.json` 统一管理登录信息，**并包含后续的 API Key 管理**。
0002 已在每个账号对象里预留了 `apiKeys: []`，但至今没有任何读写逻辑。

在托盘里做这件事的实际价值：用户想拿一枚 key 去配置客户端时，不必打开浏览器、
登录 Web 后台、翻到 Key 页面——点开账号子菜单就能看到列表并复制。

## 二、目标

在托盘子菜单里点「查看 API Key…」，弹窗列出该账号的 Key（打码展示），
可一键复制完整 Key；列表同时缓存进 `accounts.json`，断网时也能看到上次结果。

## 三、范围

**本期包含（只读 + 复制）：**

* 账号子菜单新增「查看 API Key…」；
* 面板弹窗列出该账号的 Key：名称、打码值（前 4 + 后 4）、状态、分组、最近使用时间；
* 每行「复制」按钮：通过 `GET /api/v1/keys/:id` 取完整值后写入剪贴板；
  取不到时提示「该 Key 不支持查看完整值，请到 Web 后台复制」；
* 拉取成功后把摘要写回 `accounts.json` 的 `apiKeys`（作为缓存，含 `fetchedAt`）；
* 失败分类复用 0003 的 `ApiError`（401 走恢复链、429 提示稍后、网络错误提示无法连接）。

**本期明确不做：**

* 创建 / 删除 / 改名 / 改分组 Key——破坏性操作，Web 后台更合适；
* Key 用量图表、每日用量明细；
* 把完整 Key 明文落盘（`accounts.json` 里只存打码值与元信息）。

## 四、协议（已核对后端路由）

| 用途 | 请求 |
| --- | --- |
| 列表 | `GET /api/v1/keys`（分页，`data.items[]`） |
| 详情（取完整值） | `GET /api/v1/keys/:id` |
| 创建/更新/删除 | `POST /api/v1/keys`、`PUT /api/v1/keys/:id`、`DELETE /api/v1/keys/:id`（本期不用） |

鉴权：`Authorization: Bearer <access_token>`；token 失效时复用 0004 的恢复链。

## 五、数据结构

`accounts[].apiKeys[]` 每项：

```jsonc
{
  "id": "12",
  "name": "默认 Key",
  "maskedKey": "sk-a…9f2c",
  "status": "active",
  "groupName": "默认分组",
  "lastUsedAt": "2026-10-18T09:00:00Z",
  "fetchedAt": "2026-10-18T19:00:00Z"
}
```

**不存完整 Key**；复制时临时取、只进剪贴板。

## 六、完成点

* [ ] `api.rs`：`list_api_keys(token)` / `get_api_key_value(token, id)` + 纯解析函数与单测
* [ ] `accounts.rs`：`update_api_keys(id, keys)`（原子写，保留其余字段）
* [ ] `tray.rs`：账号子菜单增加「查看 API Key…」（id `keys:<uuid>`）与指令
* [ ] `app.rs`：Key 弹窗（列表 + 复制按钮 + 加载/错误态）、请求线程与结果回传
* [ ] 剪贴板写入（eframe 已含 `clipboard` 特性），复制后 toast 提示
* [ ] 单测：打码规则、摘要解析、缓存写回后往返

## 七、验收标准

1. 操作：右键账号 → 点「查看 API Key…」→ 预期：面板弹出列表，显示名称与打码 Key，
   不出现完整 Key。
2. 操作：点某行「复制」→ 预期：剪贴板得到完整 Key，面板提示「已复制」，
   日志与 `accounts.json` 中都不出现完整 Key。
3. 操作：断网后再次打开 → 预期：显示缓存列表并标注「数据可能过期」。
4. 操作：token 失效时打开 → 预期：自动续期/重登后正常显示；彻底失效提示重新登录。
5. 操作：`cargo test` → 预期：打码、解析、缓存往返用例全部通过。

## 八、风险与回滚

| 风险 | 影响 | 应对 |
| --- | --- | --- |
| 完整 Key 泄漏到日志/文件 | 严重 | 只在内存与剪贴板中出现；日志只打印 id 与打码值；单测断言 |
| 实例不返回完整 Key | 复制失败 | 提示到 Web 后台复制，不做兜底猜测 |
| 列表很长 | 弹窗拥挤 | 固定高度 + 滚动 |
| 缓存与远端不一致 | 展示过期 | 标注 `fetchedAt`，打开时优先拉取 |

回滚：`git revert` 合并提交；`apiKeys` 字段本就预留，清空不影响其它功能。

## 九、涉及文件

* `src/api.rs`、`src/accounts.rs`、`src/tray.rs`、`src/app.rs`
* 文档：`README.md`、`AGENTS.md`、本计划

## 十、结果

> 完成后填写。
