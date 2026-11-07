<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/readme/hero-zh-dark.png">
  <img src="assets/readme/hero-zh-light.png" alt="ScreenFerry：鼠标推过去，就到了另一台">
</picture>

[![CI](https://github.com/icloudza/screen-ferry/actions/workflows/ci.yml/badge.svg)](https://github.com/icloudza/screen-ferry/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#许可证)

[English](README.md) | 简体中文

一套键盘鼠标控制多台电脑。ScreenFerry 在多台电脑之间共享键鼠（Mac、Windows 任意搭配），同步剪贴板、互传文件，连接全程 TLS 1.3 加密。

## 特性

- **键鼠共享**：光标推出屏幕边缘即切到另一台；按物理键传输，不受键盘布局和输入法影响；同步 Caps Lock，转发媒体键。
- **多台、双向**：任意一台都能当控制端，一台可以控制多台，按布局摆在上下左右。
- **剪贴板**：文字、带格式的文字、图片，光标切过去时才同步。
- **文件**：拖到屏幕边缘即发送，断线后接着传。
- **每台设备单独设置**：光标速度（DPI）、滚动方向与速度、修饰键与按键映射、防误切。
- **安全**：首次连接核对 6 位配对码，之后只认对方的证书指纹。
- **连得上**：局域网自动发现；网络不支持时按地址连接，或从控制端邀请。
- **轻量**：Rust 编写的后台常驻托盘；设置界面按需启动，关闭即退出。
- **自动更新**：每天检查一次，在后台下载好，你点“重启并更新”时安装；装之前先校验签名。
- **中英文**：界面、托盘菜单和安装程序有简体中文和英文，默认跟随系统，可以在“设置 → 外观”里切换。

## 工作方式

```mermaid
flowchart LR
  subgraph S["控制端（键鼠所在）"]
    A["后台<br/>捕获键鼠"]
    UA["设置界面"]
  end
  subgraph C["被控端"]
    B["后台<br/>注入键鼠"]
    UB["设置界面"]
  end
  UA -.-|本机通信| A
  A ==>|"输入连接<br/>键鼠、屏幕切换"| B
  A <-->|"数据连接<br/>剪贴板、文件"| B
  B -.-|本机通信| UB
```

每台电脑上常驻一个后台，负责捕获或注入键鼠、网络与同步；设置界面是独立进程，经本机回环连接后台。
输入、剪贴板和文件内容使用独立通道，避免文件内容直接排在键鼠消息前；它们仍共享网络和系统资源，大文件传输仍可能影响延迟。

首次连接时这样建立信任：

```mermaid
sequenceDiagram
  participant C as 被控端
  participant S as 控制端
  S -->> C: mDNS 广播（找不到时按地址连接，或由控制端邀请）
  C ->> S: TLS 1.3 握手，双方出示设备证书
  opt 第一次连接
    C ->> S: 配对
    Note over C,S: 两边显示同一个 6 位配对码，核对后互相记住证书指纹
  end
  C ->> S: 建立输入连接、数据连接
```

## 安装

| 系统 | 方式 |
|---|---|
| macOS | 在 [Releases](https://github.com/icloudza/screen-ferry/releases/latest) 下载 `ScreenFerry-<版本>-macos-universal.dmg`，打开后把 ScreenFerry 拖进“应用程序”。首次运行时在「系统设置 → 隐私与安全性 → 辅助功能」中允许 ScreenFerry。 |
| Windows | 在 [Releases](https://github.com/icloudza/screen-ferry/releases/latest) 下载 `ScreenFerry-<版本>-windows-x64-setup.exe` 并运行。 |

ScreenFerry 每天检查一次更新，有新版本时在后台下载好，你方便时点“重启并更新”即可；可以在“设置 → 关于”里关掉自动检查。安装前会用内置的签名公钥校验更新。

Windows 安装包：

- 没有代码签名，SmartScreen 拦截时点“更多信息 → 仍要运行”。
- 装进 Program Files，并在防火墙中放行专用网络和域网络。
- 更新时弹一次管理员确认后静默安装。另一台电脑正控制这台时，要用这台自己的键鼠点确认。卸载在“设置 → 应用”，配置和配对记录保留。

开发构建可在本仓库 **Actions → CI → 对应运行 → Artifacts** 下载：`ScreenFerry-windows-setup-<提交号>` 和 `ScreenFerry-macos-universal-dev-<提交号>`（包含 Apple 芯片与 Intel 通用版 ZIP、DMG）。推送和 PR 会自动构建，也可以点 **Run workflow** 手动触发；产物保留 14 天。Mac 开发应用采用临时签名（ad-hoc），未经 Apple 公证：移入“应用程序”后，首次打开若被系统拦截，请在本机“系统设置 → 隐私与安全性”中允许打开，再授予辅助功能权限。替换构建后可能需要重新授权。

## 使用

1. 两台电脑都打开 ScreenFerry。默认 Mac 控制、Windows 被控制，可以在“设置 → 这台电脑”里改。两台是同一种系统时，把没接键鼠的那台改成“被控制”。
2. 在被控端的“设备”页选中控制端，两边核对 6 位配对码。局域网里找不到时，用“按地址连接”，或在控制端“邀请一台电脑”；地址在对方「设备」页顶部。
3. 在控制端的“布局”页摆好各台的位置，把光标推出屏幕边缘即可切过去。

出了问题时，在“日志 → 运行日志”里导出诊断文件，[提问题](https://github.com/icloudza/screen-ferry/issues/new?template=bug_report.yml)时附上它。
设备名、用户名和文件名会换成代号。

光标在对方屏幕上时，按 <kbd>Ctrl</kbd>+<kbd>Option</kbd>+<kbd>Esc</kbd>（Windows 控制时是 <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Esc</kbd>）强制回到本机。
使用前请退出其他键鼠共享软件，避免争抢键鼠。

### 传输与响应

设置中可限制所有文件发送的总速度（MB/s，十进制；0 表示不限速），以给键鼠和其他应用留出带宽。限速不能消除无线链路本身的抖动。
“合并鼠标移动”默认关闭，开启后只合并积压的普通移动，按键、拖拽和切屏保持顺序。
总览的性能统计显示最近采样窗内的本机阶段耗时与队列情况；分位数为近似值。应用 RTT 包含两端处理、排队和网络传输，不是系统 ping 或输入到画面显示的延迟；界面区分最近采样平均、历史峰值与采样时间。
显示器查询在独立后台线程执行。性能页可查看查询、光标切换和心跳写入耗时；完整诊断文件附带有界的尖峰前后窗口，帮助定位偶发卡顿。

### 命令行

| 命令 | 作用 |
|---|---|
| `screenferry` | 常驻运行：托盘图标，打开设置界面 |
| `screenferry send <文件>…` | 把文件或文件夹发给连着的电脑 |
| `screenferry peers` / `forget <设备>` | 查看、删除已配对的设备 |
| `screenferry quit` | 退出正在运行的 ScreenFerry |
| `screenferry serve` / `connect [地址]` | 不读配置，在终端里直接运行（调试用） |

完整选项见 `screenferry --help`。

### 数据位置

| | macOS | Windows |
|---|---|---|
| 配置、配对记录 | `~/Library/Application Support/ScreenFerry/` | `%APPDATA%\ScreenFerry\` |
| 日志、运行时文件 | `~/Library/Caches/ScreenFerry/` | `%LOCALAPPDATA%\ScreenFerry\` |

## 从源码构建

需要 Rust（版本由 `rust-toolchain.toml` 固定，rustup 自动安装）和 Flutter 3.47。
macOS 另需 Xcode；Windows 另需 Visual Studio 2022（使用 C++ 的桌面开发）和 [Inno Setup](https://jrsoftware.org/isdl.php) 7.1 或更高版本。

```bash
cargo xtask bundle
```

在哪个系统上运行就打哪个系统的包，产物在 `target/bundle/`：macOS 是 `ScreenFerry.app` 和它的更新包 `ScreenFerry-<版本>-macos-<架构>.zip`（加 `--dmg` 另做安装盘），Windows 是 `ScreenFerry-<版本>-windows-x64-setup.exe`。发布用 `cargo xtask release <版本>`，见 ADR 0014。
macOS 上钥匙串里有 Developer ID 证书时自动签名（也可以用 `SCREENFERRY_SIGN_IDENTITY` 指定），否则用临时签名，每次重新打包都要重新授予辅助功能权限。

### QUIC 输入实验（可选）

已有设置默认保持 TCP。Actions 开发安装包包含 QUIC 支持，两端设置可选择 TCP、自动（优先 QUIC，失败回退 TCP）、QUIC（允许位置数据报）或纯可靠 QUIC。总览和设备页显示实际输入协议及回退原因；剪贴板和文件仍走 TCP。本地打包使用 `cargo xtask bundle --quic-experiment`（macOS 可加 `--universal --dmg`），未编译该功能的版本只提供 TCP。CLI 的 `--transport quic|auto`、`--quic-reliable-only` 继续可用。
协议、配对、顺序屏障和命令示例见 [ADR 0017](docs/adr/0017-quic-input-experiment.md)。这是开发实验，尚无真实局域网延迟收益结论。

## 开发

```mermaid
flowchart TB
  app["设置界面 · Flutter"] -.->|本机通信| L4
  subgraph L4["L4 应用"]
    daemon["ferry-daemon → screenferry"]
  end
  subgraph L3["L3 组装"]
    engine["ferry-engine"]
  end
  subgraph L2["L2 适配"]
    direction LR
    pm["platform-macos"] ~~~ pw["platform-windows"] ~~~ net["net"] ~~~ store["store"] ~~~ ipc["ipc"]
  end
  subgraph L1["L1 纯逻辑"]
    direction LR
    proto["proto"] ~~~ core["core"] ~~~ trust["trust"] ~~~ update["update"] ~~~ platform["platform（trait）"]
  end
  subgraph L0["L0 领域类型"]
    types["ferry-types"]
  end
  L4 --> L3 --> L2 --> L1 --> L0
```

每层只依赖更低的层；`unsafe` 只出现在 `ferry-platform-*` 里，按系统区分的代码只在它们和 L4 应用里。

```bash
cargo test --workspace
cargo xtask lint
```

设置界面的测试在 `app/` 下运行 `flutter test`。提交前的完整检查见 [AGENTS.md](AGENTS.md)。

| 文档 | 内容 |
|---|---|
| [AGENTS.md](AGENTS.md) | 工程规范：目录、分层、命名、测试 |
| [docs/architecture.md](docs/architecture.md) | 架构总览与 crate 依赖 |
| [docs/protocol.md](docs/protocol.md) | 线协议 |
| [docs/ipc.md](docs/ipc.md) | 设置界面与后台之间的本机通信 |
| [docs/adr/](docs/adr/) | 架构决策记录 |


本分支修复了文件重名提交、连接恢复、配对会话和后台阻塞问题，说明见 [ADR 0015](docs/adr/0015-reliable-lifecycle-and-input-isolation.md)。设置界面与后台使用本机 IPC 3，需要同包升级；设备间仍使用线协议 1。升级时请重新发送尚未完成的文件任务。Mac 上 Caps Lock 的自定义对应目前只支持保留和不发送。

## 许可证

[MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE)，任选其一。除非另有声明，你提交的贡献同样按这两种许可证授权。

## 友情链接

- [LINUX DO](https://linux.do/)
