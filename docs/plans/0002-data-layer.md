# 0002 数据层：app.json、accounts.json、凭据加密与打码

| 字段 | 内容 |
| --- | --- |
| 编号 | 0002 |
| 分支 | `feat/data-layer` |
| 状态 | 已完成（待合入 dev） |
| 提出日期 | 2026-10-18 |

## 一、背景

目前工程只有骨架：托盘图标 + 面板 + 中文字体，没有任何持久化，也还没有登录。

需求方已明确后续要多账号登录，并给出两条硬约束：

1. 登录信息统一由**一个** `accounts.json` 管理，将来 API Key 也放进去；
2. 环境参数与默认设置放同目录的 `app.json`，其中一项是"账号是否打码展示"。

如果先写登录 UI 再补存储，登录逻辑会被临时结构绑死（token 放哪、密码怎么加密、账号怎么排序
都会在 UI 里散落），因此**先做数据层**，让 0003/0004 只依赖稳定接口。

## 二、目标

程序启动后能在**可执行文件所在目录**读写 `app.json` 与 `accounts.json`：
读取配置、增删改账号、密码与 token 加密落盘、账号名按开关打码，
并且在文件损坏或目录不可写时给出确定的、可观测的行为。

## 三、范围

**包含：**

* 数据目录解析：`current_exe()` 所在目录；启动时做**可写探测**，不可写时返回明确错误；
* `app.json`：schema、默认值、缺失时自动创建、解析失败时的行为；
* `accounts.json`：schema（含为 API Key 预留的 `apiKeys`）、账号 CRUD、按 `createdAt` 升序排序、
  `baseUrl + email` 唯一性校验；
* 凭据加密：Windows 用 DPAPI（`CryptProtectData`），macOS 用钥匙串（Keychain）保存密文/条目；
  解密失败一律视为"凭据损坏"，要求重新登录，不猜测、不降级为明文；
* 打码函数 `mask_email`：只影响展示，绝不影响存储与请求；
* 原子写入（临时文件 + 重命名）与损坏文件隔离（改名备份后重建空文件）；
* 启动日志：打印实际数据目录、账号数量、app.json 中的生效配置。

**不包含（明确排除）：**

* 登录 UI、登录请求、2FA 流程 → 0003；
* 托盘账号子菜单、状态刷新、删除确认交互 → 0004；
* API Key 的实际增删改查（只在 schema 里预留字段）；
* 单实例互斥（两个进程同时写同一个 accounts.json 的竞态）→ 见风险，后续计划；
* app.json 热加载（改文件后需重启生效，本计划明确写死这一点）；
* Linux 支持。

## 四、数据结构（本计划的核心产出，需评审）

### 4.1 `app.json`

```jsonc
{
  "schemaVersion": 1,
  // 账户名是否打码展示（只影响 UI，不影响存储与请求）
  "maskAccounts": false,
  // 后台刷新登录态与余额/订阅摘要的间隔，单位秒
  "refreshIntervalSeconds": 300,
  // 单次 HTTP 请求超时，单位秒
  "requestTimeoutSeconds": 10
}
```

* 文件不存在 → 用默认值创建并写盘；
* 字段缺失 → 用默认值补齐（前向兼容），**不**整份重置；
* JSON 解析失败 → 备份为 `app.json.corrupt-<时间戳>`，用默认值重建，并在日志与面板提示；
* 非法值（如 `refreshIntervalSeconds <= 0`）→ 回落到默认值并 `warn`。

### 4.2 `accounts.json`

```jsonc
{
  "schemaVersion": 1,
  "accounts": [
    {
      "id": "8f14e45f-...",            // UUID v4，创建后不变
      "baseUrl": "http://10.0.0.5:8080", // 已归一化：去尾部斜杠、校验 http/https
      "email": "user@example.com",
      "role": "admin",                  // "admin" | "user" | "unknown"，来自 /auth/me
      "secrets": "enc:v1:<base64>",     // 加密块，见 4.3；Windows 为密文，macOS 为钥匙串引用
      "createdAt": "2026-10-18T10:00:00Z", // 展示顺序依据（升序）
      "lastLoginAt": "2026-10-18T10:00:03Z",
      "tokenExpiresAt": "2026-10-19T10:00:03Z",
      "apiKeys": []                     // 预留给后续 API Key 管理
    }
  ]
}
```

* 展示顺序 = `createdAt` 升序；`createdAt` 由程序写入，不接受手改排序；
* 唯一性：`baseUrl`（归一化后，忽略大小写与尾斜杠） + `email`（忽略大小写）不得重复；
* `secrets` 解密后的明文结构（**不出现在文件里**）：
  `{"password": "...", "accessToken": "...", "refreshToken": "..."}`；
* 账号被删除时，macOS 还需同步删除钥匙串条目（避免残留）。

### 4.3 加密方案

抽象成 `SecretStore`，两个平台各自实现，调用方只看到 `seal(account_id, plaintext) -> String`
与 `open(&str) -> Result<String>`：

| 平台 | 实现 | 文件里的 `secrets` 字段 | 说明 |
| --- | --- | --- | --- |
| Windows | DPAPI `CryptProtectData`（当前用户作用域） | `enc:dpapi:v1:<base64 密文>` | 换个 Windows 用户或换台机器都解不开 |
| macOS | 钥匙串条目（service=`sub2api_tray`，account=`<uuid>`） | `enc:keychain:v1:<uuid>` | 密文进钥匙串，文件里只有引用；删除账号要清条目 |

* 解密失败 → 返回 `Err`，调用方把该账号标记为"凭据损坏，请重新登录"，**不**清空其他账号；
* 绝不把明文（密码、token）写进日志；日志只允许出现账号 id 和打码后的邮箱；
* macOS 分支本计划**实现但不在实机验收**（约定见 AGENTS.md 第十节）。

### 4.4 打码规则（`maskAccounts = true` 时生效）

只处理邮箱，保留首尾可辨识性：

| 输入 | 输出 |
| --- | --- |
| `a@b.cn` | `a***@b***.cn` |
| `ab@cd.com` | `ab***@cd***.com` |
| `zhangwei@example.com` | `zh***@ex***.com` |
| `user@sub.example.com` | `us***@su***.example.com`（域名保留第一段与其余部分） |
| 非法/空 | `***` |

规则要点：本地部分保留前 2 个字符（不足 2 个则保留 1 个）后接 `***`；域名第一段同理，
其余层级原样保留；URL **不打码**（按需求只遮账号名）。

## 五、完成点

* [x] `src/paths.rs`：解析 exe 目录、可写探测（写临时文件后删除）、返回 `Result<PathBuf>`
* [x] `src/config.rs`：`AppConfig`（serde）、默认值、缺失字段补齐、坏文件隔离、非法值回落
* [x] `src/accounts.rs`：`AccountsFile`/`Account` 结构、`load`/`save`（原子写）、
      `add`/`remove`/`find`、唯一性校验、按 `createdAt` 排序、损坏隔离
* [x] `src/secrets.rs`：`SecretStore` 抽象 + Windows DPAPI 实现 + macOS 钥匙串实现
* [x] `src/mask.rs`：`mask_email` 及其单元测试
* [x] 启动时初始化：解析目录 → 探测可写 → 加载/创建两个文件 → 打印日志
* [x] 新增依赖并在 `AGENTS.md` 技术栈表登记：`serde`、`serde_json`、`uuid`、`time`、`base64`、
      （Windows）`windows-sys`、（macOS）`security-framework`，开发依赖 `tempfile`
* [x] 单元测试：打码 7 例、排序、去重、schema 往返、损坏隔离、原子写、DPAPI 往返（Windows）
* [x] 在 `AGENTS.md` 阶段表里把数据层标记为已完成（阶段 1a）

> 计划外的补充（见“结果”里的偏差记录）：新增 `src/data.rs` 门面模块；
> 面板改为“顶/底固定 + 中间滚动”；调试开关在生效时不再因失焦收起。

## 六、验收标准

1. 操作：把编译产物放到一个**空目录**并运行 → 预期：该目录下出现 `app.json` 与
   `accounts.json`，内容分别符合 4.1/4.2 的 schema（`schemaVersion` 为 1，`accounts` 为 `[]`）。
2. 操作：删除 `app.json` 后重启 → 预期：自动重建默认配置，日志出现"已创建默认 app.json"。
3. 操作：把 `app.json` 改成非法 JSON 后重启 → 预期：原文件被改名为
   `app.json.corrupt-<时间戳>`，程序用默认配置正常启动，日志有 `warn`。
4. 操作：把 `app.json` 的 `maskAccounts` 改成 `true`、`refreshIntervalSeconds` 改成 `0`
   （`u64` 无法表示负数，写 `-5` 属于类型错误，会走第 3 条的整份隔离路径）→ 预期：
   打码开关生效；非法间隔回落到 300 并在日志中 `warn`。
5. 操作：把 `accounts.json` 改成非法 JSON 后重启 → 预期：原文件被隔离备份，账号列表为空，
   程序不崩溃，日志说明原因。
6. 操作：`cargo test` → 预期：打码、排序、去重、往返、损坏隔离、原子写用例全部通过；
   Windows 上 DPAPI 往返用例通过。
7. 操作：把程序放到不可写目录（如 `C:\Windows\System32` 下的普通用户）运行 → 预期：
   日志与返回值明确报"数据目录不可写"，**不**创建文件、**不**静默换目录；
   登录入口在 0003 落地前保持禁用。
8. 操作：`cargo clippy --all-targets -- -D warnings` → 预期：零警告。

## 七、测试方式

**自动化：**

* `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo build`；
* `cargo test`：新增 `mask` 6 例、`accounts` 排序/去重/往返/损坏隔离、`paths` 可写探测、
  `secrets` 的 DPAPI 往返（`#[cfg(windows)]`）；文件类测试统一用 `tempfile::TempDir`，
  **不得**污染 exe 目录。

**手工验证：**

* Windows：按第六节 1～5、7 逐条执行，并检查日志不含明文密码/token；
* macOS：仅要求 `cargo check --target aarch64-apple-darwin` 通过（无实机，按约定延后验收）。

## 八、风险与回滚

| 风险 | 影响 | 应对 |
| --- | --- | --- |
| 两个进程同时写 accounts.json | 后写覆盖先写，可能丢账号 | 原子写降低"半截文件"概率；单实例互斥单独开计划 |
| exe 放在 Program Files 等只读目录 | 首次启动即失败 | 明确报错并提示换目录（已确认不做静默回退） |
| macOS 钥匙串在未签名 app 上弹授权 | 用户需点"允许" | 已在文档标注；验收延后 |
| DPAPI 换用户/换机器后解不开 | 账号显示"凭据损坏" | 设计如此，提示重新登录，不删除记录 |
| 手改 accounts.json 写坏结构 | 启动即空列表 | 损坏隔离 + 备份，原文件仍可人工修复 |
| `createdAt` 被手工改乱 | 展示顺序异常 | 加载时对缺失/非法 `createdAt` 回落到"文件顺序"并 `warn` |

回滚方式：本计划只新增模块与依赖，不改动现有 UI；`git revert` 合并提交即可。

## 九、涉及文件

* 新增：`src/paths.rs`、`src/config.rs`、`src/accounts.rs`、`src/secrets.rs`、`src/mask.rs`、
  `src/data.rs`（计划外补充：数据层门面）
* 修改：`src/main.rs`（启动时初始化数据层并记录日志）、`src/app.rs`（数据目录提示、
  红色错误横幅、面板滚动布局）、`Cargo.toml`（依赖）
* 文档：`AGENTS.md`（技术栈表、阶段表、数据文件章节）、`README.md`（数据文件说明）、
  `docs/panel-preview.png`（面板截图更新）
* 测试：各模块内 `#[cfg(test)] mod tests`

## 十、后续计划依赖（预告）

| 编号 | 内容 | 依赖本计划的部分 |
| --- | --- | --- |
| 0003 | 登录弹窗：面板顶部按钮、链接/账户/密码、2FA 手输验证码、三类错误提示 | 用 `accounts.add` 落库、`SecretStore` 存密码与 token、`mask_email` 展示 |
| 0004 | 托盘账号子菜单、状态刷新、删除确认、排序 | 用 `accounts.load` 排序结果渲染、`remove` 删记录并清钥匙串 |

## 十一、结果

* 完成日期：2026-10-18
* 分支：`feat/data-layer`

### 验证记录

**自动化**

* `cargo fmt --check` → 通过；
* `cargo clippy --all-targets -- -D warnings` → 通过，零警告；
* `cargo test` → **44 passed / 0 failed**（新增：`mask` 7 例、`paths` 6 例、`config` 5 例、
  `accounts` 13 例、`secrets` 5 例、`data` 2 例，其余为 0001 的定位用例）；
* `cargo build` → 通过。

**Windows 实机手工验证**（2560×1440，100% 缩放）

1. 把 exe 单独拷到空目录运行 → 该目录生成
   `app.json`（`schemaVersion=1, maskAccounts=false, refreshIntervalSeconds=300,
   requestTimeoutSeconds=10`）与 `accounts.json`（`schemaVersion=1, accounts=[]`）；
   启动日志打印数据目录与"已加载 0 个账号"。
2. 删除 `app.json` 后重启 → 日志 `已创建默认配置文件`，文件重建为默认值。
3. 把 `accounts.json` 写成非法 JSON 后重启 → 日志
   `accounts.json 解析失败（expected value at line 1 column 1），将备份后用空列表重建`，
   原文件被改名为 `accounts.json.corrupt-1792263620`，新文件为
   `{"schemaVersion":1,"accounts":[]}`，程序正常启动。
4. 用 `icacls /deny <用户>:(W)` 造出一个只读目录后运行 → 日志
   `数据层初始化失败：数据目录不可写：…: 拒绝访问。 (os error 5)`；
   目录内**只有 exe**，没有生成任何数据文件；面板顶部出现红色提示
   “数据目录不可写，账号无法保存 / 请把程序移动到可写目录…后重启”。
5. `maskAccounts=true` 的打码效果由单元测试 `data::tests::打码开关通过数据层生效` 与
   `mask::tests::*` 覆盖（界面展示留到 0004 验收）。
6. 日志中未出现任何明文密码或 token（`Credentials` 的 `Debug` 只打印长度）。

### 与计划的偏差

1. **新增 `src/data.rs`**：计划只列了 5 个数据模块。实现时需要一个把"目录 + 配置 + 账号 +
   凭据存储"组装起来的门面，否则 `main` 与后续的 `app`/登录流程会各自拼装一遍。
2. **验收标准 4 的样例从 `-5` 改为 `0`**：`refreshIntervalSeconds` 是 `u64`，
   写负数会在 JSON 反序列化阶段就失败，走"整份隔离"而不是"字段回落"。
3. **面板布局重构**：加红色横幅后原布局把页脚挤出面板，改为"顶部固定 + 底部固定 +
   中间 ScrollArea"，并给页脚加不透明底色，避免滚动内容与页脚视觉重叠。
4. **调试开关增加"不自动收起"**：自动化截图时窗口拿不到焦点，原逻辑会让面板在截图前消失。
5. **`accounts.rs`/`secrets.rs` 顶部临时 `#![allow(dead_code)]`**：这批 API 的调用方在
   0003/0004 才出现，先把噪声压掉；两个计划接入后必须删掉这两行。

### 遗留问题

* macOS 的钥匙串实现只做了 `cargo check` 层面的编译验证，未实机运行（按用户确认延后）；
* 单实例互斥未做：两个副本同时运行会互相覆盖 `accounts.json`（已在 README 记录）；
* API Key 字段只做了 schema 预留，没有读写接口（0005 待规划）。
