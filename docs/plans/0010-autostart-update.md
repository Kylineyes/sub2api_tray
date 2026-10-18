# 0010 开机自启、版本检查、点击复制 Key

| 字段 | 内容 |
| --- | --- |
| 编号 | 0010 |
| 分支 | `feat/autostart-update` |
| 状态 | 已完成（待合入 dev） |
| 提出日期 | 2026-10-18 |
| 依赖 | 0009（设置页） |

## 一、背景

阶段 3 剩下的两项（开机自启、自动更新检查）加上一个交互诉求：
Key 明细里的每一块应当**点一下就复制**，并在方块上方盖一个"已复制"浮窗。

## 二、范围

* **开机自启**：Windows 写 `HKCU\...\CurrentVersion\Run`（`reg.exe`，免管理员权限）；
  macOS 写 `~/Library/LaunchAgents/com.sub2api.tray.plist`（RunAtLoad）。
  命令行带 `--hidden`，登录时静默启动不弹面板。设置页勾选即时生效（不需要保存按钮）。
* **版本检查**：`GET https://api.github.com/repos/<owner>/<repo>/releases/latest`，
  比较 `tag_name` 与本地 `APP_VERSION`（逐段数字比较，预发布后缀忽略）；
  发现新版本时在设置页显示版本号并提供"复制下载链接"。
  **不自动下载/替换二进制**（签名、权限、运行中的 exe 被占用等风险）。
* **点击复制 Key**：点击面板内 Key 方块 → 后台 `GET /api/v1/keys/:id` 取完整值 →
  写剪贴板 → 该方块上方浮出"已复制"胶囊，约 1.6 秒后消失。
* **展开三角加强**：`▸/▾`（10pt 灰）改为 `▶/▼`（12pt 加粗、品牌色）。

## 三、验收标准

1. 设置页勾选"开机自动启动" → 预期：`HKCU\...\Run` 出现 `sub2api_tray` 值，
   指向当前 exe 且带 `--hidden`；取消勾选后该值消失。
2. 重启登录后 → 预期：程序自动在托盘运行，面板不自动弹出。
3. 点"检查更新" → 预期：网络正常时给出"已是最新/发现新版本"结论；
   仓库没有 Release 时提示"远端还没有发布记录"（GitHub 返回 404 时按此处理）。
4. 点击某个 Key 方块 → 预期：剪贴板得到完整 Key，方块上方出现"已复制"胶囊并自动消失。
5. 展开箭头 → 预期：为品牌色加粗的 ▶/▼，明显可见。
6. `cargo test` → 全部通过（含版本比较 2 例）。

## 四、结果

* `cargo fmt --check` / `clippy -D warnings` / `cargo test`（**93 passed**）/ `build` 全绿；
* 实测 `https://api.github.com/repos/Kylineyes/sub2api_tray/releases/latest` 当前返回 **404**
  （仓库未公开或还没有 Release），因此"检查更新"会走 `Ok(None)` → 提示"远端还没有发布记录"。
  要让该功能真正可用，需要在 GitHub 上打一个 Release Tag，并确保仓库公开可读。
* 开机自启、点击复制的交互需人工在 Windows 上确认（无法自动化点击托盘面板）。

## 五、遗留

* 自动下载安装更新未做（只提示 + 复制链接）；
* macOS 的 LaunchAgent 未实机验证。
