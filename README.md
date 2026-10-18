# Sub2API 托盘工具

常驻 Windows 托盘的小面板：**不用开浏览器**，随时看到已登录账号在不在线、余额与订阅还剩多少，
并直接取用账号的 API Key。

![面板](docs/panel-preview.png)

## 快速开始

* **下载**：从 [Releases](https://github.com/Kylineyes/sub2api_tray/releases) 取
  `sub2api_tray-v*-windows-x64.zip`，解压后双击 `sub2api_tray.exe`。
* **从源码**：需要 Rust 1.95+ 与 MSVC 工具链，`cargo run` 即可（启动后面板默认隐藏，点托盘图标打开）。

首次运行会在 exe 同目录生成配置文件与账号文件；整个程序是绿色的，删掉目录即卸载。

## 能做什么

* **账号**：面板顶部「登陆新用户」填地址、账号、密码即可登录（支持两步验证），
  同时最多保留 10 个账号；账号的查看、Key 与「退出账号」都在面板里完成（右键菜单只剩：立即刷新 / 版本 / 退出）。
* **状态**：后台自动刷新在线状态、余额、订阅用量；托盘图标右下角的小圆点用
  **绿 / 黄 / 红** 表示"全部在线 / 有账号离线 / 有账号凭据失效"，也可以在菜单或面板里「立即刷新」。
* **API Key**：点开账号即展开它的全部 Key，显示已用/总额度、最近用量与所属分组；
  **点任意 Key 就把完整值复制到剪贴板**，方块上会浮出「已复制」。断网时显示上次缓存的列表。
* **设置**：右上角齿轮里可改账号名打码、刷新间隔、请求超时，开关开机自启、检查新版本。
* **记住状态**：哪个账号是展开的会被记住，重启应用或重启电脑后依然是展开的样子。

## 界面

| 收起（只显示账号与地址） | 展开（Key 明细） |
| --- | --- |
| ![收起](docs/panel-collapsed.png) | ![展开](docs/panel-preview.png) |

## 构建

```powershell
cargo run                 # 调试运行
cargo build --release     # 产物：target\release\sub2api_tray.exe
```

macOS 代码路径已实现但**尚未实机验收**，暂不提供构建产物。

## 发布

在 `main` 上打 `v*` tag 即触发 GitHub Actions：跑格式检查、clippy 与单元测试，
编译 Windows x64 版本并自动挂到该 tag 的 Release。

```bash
git tag v0.2.0
git push origin v0.2.0
```

## 开发

分支模型、提交规范、注释要求、模块边界等约定见 [AGENTS.md](AGENTS.md)。

## 许可证

[MIT](LICENSE)
