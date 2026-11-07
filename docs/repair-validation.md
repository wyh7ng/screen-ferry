# 修复与验证记录

首批记录日期：2026-11-05；第二批文件补充日期：2026-11-06。基线：`fc5e14378219c294cb88a73b1bb87edfd6843753`。
工作分支：`codex/reliability-fixes`。代码位于 `/Users/wyh/Desktop/codex/screen-ferry-fixes`，当前保留为未提交改动，供审核。

## 首批已完成

- 文件原子不覆盖提交、最长文件名编号、跨盘临时复制、部分提交失败保留已公开文件。
- 文件独立连接重试、幂等通告/接受、完成回执重放、EOF/取消乱序处理、清单与并发预算、失败错误脱敏。
- 双向每设备文件/剪贴板策略；数据连接独立建立与恢复；恢复时避免旧剪贴板覆盖活动端内容。
- 专用剪贴板线程，平台同步读写和图片转换不再占用输入执行线程。
- 浮层过期计时忙等修复、按帧截止绘制、静止跳过场景构建、合并原生刷新。
- Windows 光标句柄复用、Mac 看守线程复用、多对一按键映射持有计数；Mac Caps 保持型映射采取平台限制和旧配置回退。
- IPC 3 会话/请求绑定、配对断线重建、异步结果按窗口路由、失败回滚、Welcome 超时、强调色范围回退。
- 后台单实例锁、握手任务回收、旧引擎事件失效、后台邀请任务管理、固定主机名异步解析。
- 文件准备、日志读取、诊断与预览后台任务化；日志每窗口单任务，预览串行合并；更新能力和平台信息缓存。
- 更新失败状态及退避、Windows 锁定句柄后重新验包、Mac 跨 IPC 版本健康探测及改名应用更新。

## 首批检查结果（历史记录，2026-11-05）

本表对应首批修复快照，不表示后续第二批全部改动已经通过相同的全项目检查。

| 检查 | 结果 |
|---|---|
| `cargo fmt --all --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `cargo test --workspace` | 383 通过，0 失败，2 个依赖局域网组播的测试按原配置忽略 |
| `cargo xtask lint` | 14 个 crate 均通过 |
| `cargo deny check` | advisories、bans、licenses、sources 均通过；存在仓库已有重复依赖提示 |
| `cargo check --workspace --all-targets --target x86_64-pc-windows-gnu` | 通过；交叉类型/编译检查，不是 Windows 执行测试 |
| Windows 平台 all-targets 交叉 Clippy | 通过 |
| Flutter 格式与 `flutter analyze` | 通过 |
| `flutter test` | 124 通过 |

Rust 为仓库固定的 1.91.0，Flutter 为 CI 使用的 3.47.8。工具位于 `/Users/wyh/.cache/screen-ferry-tooling/`；
Rust 命令使用该目录下的 cargo/rustup，未修改用户 shell 初始化文件。为 Windows 交叉检查安装了 MinGW。

文件测试使用真实回环 TLS 和可控故障，不靠大文件传输速度碰巧产生中断；浮层到期测试使用可控时间。
macOS 原子提交已执行并发碰撞与目录测试；Windows 同类测试及更新文件锁测试已交叉编译，仍需 Windows 环境执行。

## 首批交付与验证边界

- 当前交付为源码修复，没有安装到正在使用的 ScreenFerry，也没有发布远端版本。
- UI 与后台需使用同一构建（IPC 3）；设备间线协议仍为 1。两台都升级可取得完整故障恢复行为。
- 原始版本的 `partial/<传输号>/` 不迁移。第一批修复后的设备隔离缓存与第二批的兼容行为见下文，不能笼统写成所有旧缓存都不可复用。发送端待发送任务只在内存，发送进程重启后仍需重新发起任务。清理完成回执会移除相应去重信息。
- 原子文件改名与回执落盘之间仍不具备跨目录掉电事务保证；不能将断线恢复承诺扩大成任意断电均完全事务化。
- 已经进入的系统剪贴板调用不能强行中断；取消会丢弃未执行请求，输入线程继续运行。
- 首批自动检查未覆盖 Mac/Windows 实机辅助功能、实际键鼠、UAC、拖放与休眠唤醒验收，以及用户 Wi-Fi 环境的 p95/p99 对照。这些是证据边界，不是本轮要求用户继续操作的前置条件。

首批保留 TCP，没有实现 QUIC、输入移动合并、传输限速、日志增量化与更细的 UI 重建优化。后续实现进度与本表的历史检查结果分开记录。

实现边界和兼容性详见 [ADR 0015](adr/0015-reliable-lifecycle-and-input-isolation.md)。

## 第二批文件实现（2026-11-06）

- `Files::set_rate_limit(u64)` 在线设置总文件内容发送字节预算；默认 0 不限速。所有设备和并发任务共享 FIFO 队列，动态分块、在线降速、取消不占用输入线程；协议开销、接收流量与物理带宽不在此预算保证范围内。
- 完成回执按 7 天 / 最近 1024 份保留策略后台清理，启动、每次完成和每小时触发。清理跳过活动任务、partial 内容及未完成提交意图，只删除缓存，不删除下载内容。
- 提交前持久化清单指纹、源/目标、稳定文件身份与内容摘要；名称碰撞和跨卷只更新单项意图。恢复确认已经公开的同一对象，避免改名成功而回执尚未落盘时重复提交。
- 跨卷复制前登记私有容器身份，内容与目标卷来源记录同步后再提交；新引擎实例可从磁盘恢复。用户修改或替换目标时保留内容和恢复记录并报冲突；多项提交后续失败保留已经公开的文件。
- 新增平台文件身份、已打开文件句柄身份、原子记录替换及目录同步接口。Mac 使用卷/文件身份并尽可能同步目录；Windows 使用卷序列号/文件 ID 与 `WRITE_THROUGH`，不伪称支持目录 fsync。
- 准备清单稳定排序、规范化源路径去重、清洗后名称编号复用递增游标；扫描与错误数量有预算，汇总错误而不发送部分准备结果，错误内容不暴露源路径。

### 实际已执行的文件范围检查

| 检查 | 结果与范围 |
|---|---|
| `cargo test -p ferry-engine --test suite files -- --nocapture` | 38 通过，0 失败；过滤名也匹配部分已有 edge_drop / diagnostics 测试，不是 38 项全部为新增文件测试 |
| `cargo test -p ferry-platform-macos file_commit -- --nocapture` | 4 通过：原子碰撞、目录不覆盖、稳定句柄身份、记录替换；实际在 Mac 本地文件系统执行 |
| `cargo clippy -p ferry-engine -p ferry-platform-macos -p ferry-platform-mock --all-targets -- -D warnings` | 通过，针对当时整合快照的上述目标；不是第二批全 workspace 最终检查结论 |
| `cargo check -p ferry-platform-windows --all-targets --target x86_64-pc-windows-gnu` | 通过；类型/编译检查，没有执行 Windows 测试 |
| `cargo clippy -p ferry-platform-windows --all-targets --target x86_64-pc-windows-gnu -- -D warnings` | 通过；同样不是 Windows 原生执行结果 |
| 文件范围 `rustfmt` 与 `git diff --check` | 通过；未据此宣布第二批全项目发布就绪 |

该记录沿用前述隔离工具链。第二批其他模块、全 workspace 最终检查与打包状态由后续整合记录给出，不能把首批 383/124 的历史数字直接搬作第二批结果。

### 证据与保证范围

| 承诺 | 当前证据 | 尚不能推出的结论 |
|---|---|---|
| 共享限速、公平与在线调整 | `files/budget.rs` 用暂停时钟验证 FIFO 总预算、调高/调低、取消排队；真实回环 TLS 测试确认 1 B/s 的文件取消及时结束 | 未做真实 Wi-Fi 吞吐误差、并发公平性长期压测；不等于输入延迟下降或物理网络隔离 |
| 原子不覆盖与跨卷提交 | 已有并发同名、目录哨兵、Mac 原生改名测试；注入 `CrossesDevices` 验证 staging 路径与清理 | 注入跨卷错误不等于真实不同卷/不同文件系统掉电测试；Windows 原生测试仅交叉编译 |
| 改名后恢复 | 在改名已成功、后续持久化失败处注入故障；销毁旧引擎，新 `Files` 实例仅靠磁盘与配对恢复；同卷/跨卷各验证不生成第二份 | 不是拔电、杀进程或文件系统写入重排实验；不保证所有文件系统具有跨目录事务性 |
| 目标用户修改保护 | 恢复前修改目标内容，验证不覆盖、不删除、不另存第二份；部分提交后用户修改的文件也保留 | 尚无针对所有目标移动/同内容替换/复杂目录变更时序的独立回归，相关分支不能写成全部实测 |
| 回执清理 | 人工过期与超过 1024 份记录、活动任务、partial 和未完成意图保护测试 | 未做真实持续运行 7 天；定时扫描是异步策略，磁盘故障或保护中的任务可使实际回收延后 |
| 清单稳定去重与失败汇总 | 重复源/不同源同名、两处准备失败且不发送有效项的回归；首批保留长文件名编号与路径校验测试 | 未做 100,000 项性能基准或大目录扫描中取消的专项压力测试，不能宣称具体耗时/内存收益 |
| 文件生命周期 | 保留独立 File TLS 建连失败、重复 Offer、最终确认丢失、EOF 先于 Cancel、确定性整会话续传与取消回归 | 自动时序注入不代替 Mac/Windows 原生拖放、网络或 UAC 验收 |

### 缓存兼容与恢复边界

- 原始基线的 `partial/<id>/` 缺少设备指纹隔离，不自动迁移；第一批的 `partial/<fingerprint>/<id>/state` 续传格式仍读取，旧二元素完成回执仍读取并以文件修改时间参与清理。该兼容来自实现审查，目前没有单独的旧回执格式升级回归。
- 新的 `commit-intent.json` / `commit-items/` 是本地格式，不更改设备间协议 1。旧程序不理解它们，因此不承诺降级后保持第二批恢复语义。
- 恢复发生在同一设备以同一任务 ID、匹配清单再次通告时。接收端新进程能恢复记录，不等于发送端进程重启后能找回仅在内存中的发送任务。
- 完成回执被清理、手工删缓存或使用新任务 ID 后，不再承诺历史去重。7 天 / 1024 份限制针对可清理完成回执，不能据此强删活跃任务或未完成意图；每小时触发意味着并非到期瞬间回收。
- 内容与记录使用同步写入、原子替换和可用的平台持久化请求，但文件系统语义、硬件缓存与系统故障仍影响掉电结果。已缩小并可恢复“改名成功、回执未落盘”的可识别窗口，仍不宣称任意掉电完全事务化。

文件线上行为与本地恢复顺序见 [protocol.md](protocol.md) 第 6 节，决策见 [ADR 0016](adr/0016-bounded-input-and-performance-observability.md)。本次文档复核没有重新执行大范围测试或原生双机验收。


## 第二批整合时的显示器环境限制

2026-11-06 首次全工作区运行在原有 `reports_one_primary_display_with_real_size` 失败：CoreGraphics返回0个活动显示器。只读系统信息确认PHL显示器 `spdisplays_asleep=yes`，显示器实现本轮没有改动。该测试仍保留原断言，标为需要唤醒桌面的显式原生测试，避免无头CI/显示器睡眠把环境前置条件当代码失败；未唤醒设备、未继续人工验收。后续全套中的跳过数量须包含这一项，不能称该项已通过。可在未来具备条件时显式执行 `cargo test -p ferry-platform-macos reports_one_primary_display_with_real_size -- --ignored`。


## 第二批最终整合（2026-11-06）

首批和后续常规优化、QUIC可选实验已合并在当前工作副本，未提交/推送/发布，完整要求与证据映射见 [development-completion.md](development-completion.md)。人工验收按用户要求停止，本节均为开发过程的自动检查。

| 最终检查 | 结果 |
|---|---|
| cargo fmt --all --check；git diff --check | 通过 |
| cargo clippy --workspace --all-targets -- -D warnings | 通过 |
| cargo clippy --workspace --all-targets --all-features -- -D warnings | 通过 |
| cargo test --workspace | 438通过，0失败，3跳过 |
| cargo test --workspace --all-features | 448通过，0失败，3跳过 |
| cargo xtask lint | 14 crate通过 |
| cargo deny check | advisories/bans/licenses/sources通过；重复依赖及未使用许可白名单提示，无阻断 |
| Windows全workspace/all-targets/all-features交叉Clippy | 通过（x86_64-pc-windows-gnu，不是Windows原生执行） |
| Dart格式与Flutter analyze | 通过 |
| Flutter完整测试 | 133通过 |
| macOS release后台（含可选QUIC） | 构建通过，默认传输仍TCP |
| Windows x64 GNU release后台（含可选QUIC） | 交叉链接通过，尚未在Windows运行本次产物 |

三项跳过为两项既有组播测试、一项需要唤醒显示器的原生测试（环境说明见上节）。拆分resident模块后，Flutter短语覆盖测试已改为递归扫描整个resident源码目录，继续检查所有后台短语，不放宽原覆盖条件。

本轮完成：输入阶段指标与队列背压/可选合并、全局文件预算与提交恢复/回执清理、剪贴板共享缓存、日志有界后台写入与分页游标、UI分域刷新、后台职责拆分、QUIC可靠与DATAGRAM实验及回退。新配置schema7默认不限速/不合并，默认与常驻传输TCP；界面和后台仍需同构建IPC3。物理网络尾延迟收益、Windows原生执行、完整应用包/更新/休眠等不从这些检查推断。

检查日志在 `/tmp/screen-ferry-final-*.log`。最新开发二进制与来源摘要保存在 `target/development/`；不与先前首批验收包混淆。


## 安装版传输选择与DMG修复（2026-11-06）

用户安装5c4ccf6后发现当前输入仍TCP：该包未编译QUIC feature且原常驻入口固定TCP，属于原CLI-only设计，不是自动回退。现补schema8 input_transport四模式、设置界面、resident入口、构建能力与实际协议/回退状态，Actions开发包带quic-experiment。原配置默认TCP，当前已安装应用未修改或切换。

DMG确认hide_extensions使dmgbuild在签名后写入FinderInfo。移除该设置，并在制盘后只读挂载、外层/helper严格验证、卸载；错误删除无效产物。用旧版本干净ZIP副本重新制盘回归成功，坏盘与非映像两种负向场景均拒绝且清理，无需修改用户安装应用。

本地整合：默认Rust445通过、全feature455通过，均3环境依赖跳过；Flutter139通过；默认/全featureClippy、Windows全feature交叉Clippy、结构lint与cargo-deny通过。配置升级的“未来版本拒绝”测试改为SCHEMA_VERSION+1，避免每次升级遗漏硬编码测试号。新增控制端兼容TCP的语义回归已通过：QUIC/纯可靠QUIC控制端均可接受已配对TCP客户端，双方实际=tcp且无虚构回退原因，剪贴板与文件完整提交；该项在455项整合检查后单独运行通过。

延迟排查：90秒同步网关/Windows ICMP时应用RTT最高9.712ms；90秒纯被动出现110.984ms；再90秒同步ICMP最高12.089ms。三段本机调度最大均约3–4ms，两段Ping无丢包。现有证据不能确认Wi-Fi省电、远端调度、重传或队列中的单一根因，也不证明QUIC改善。具体本机观察文件保存在workspace外，不上传用户网络/运行数据到仓库。

## 应用延迟第一批：显示器查询与诊断（2026-11-07）

见[ADR0019](adr/0019-display-query-isolation-and-latency-context.md)。显示器启动/周期查询已移到有界串行线程，重连准备在创建网络连接前完成；保留无显示器先配对的旧行为。新增查询/光标切换/心跳写入指标、诊断最近窗口及4次尖峰上下文、总览样本年龄及明确应用RTT语义。

| 检查 | 结果 |
|---|---|
| Rust默认工作区 | 457通过，0失败，3忽略 |
| Rust全feature工作区 | 468通过，0失败，3忽略 |
| 默认/全feature全target Clippy | 通过 |
| Windows GNU全feature全target交叉Clippy | 通过；不是原生执行 |
| Rust格式、git diff、14 crate结构lint、cargo-deny | 通过；deny保留已有非阻断提示 |
| Flutter完整测试 | 141通过；最终文案与共享fixture变更后相关用例另行通过 |
| Flutter analyze、Dart格式 | 通过 |

三项忽略仍为两项既有组播、一项需活动显示器的原生测试，未把忽略算作通过。首次完整运行暴露配对测试使用空显示器的问题，修复了预检提前拒绝改变配对语义的实现，没有扩大测试超时或修改fixture规避；随后两套完整工作区通过。

新增显示器测试覆盖阻塞超过1秒期间真实TLS心跳和按键Up继续进展、不补排查询、周期失败/空布局保留缓存及恢复、启动取消、跨会话旧结果丢弃。另有两端指标计数、IPC缺省字段、诊断内存/过期/生命周期/实际导出与UI采样时间回归。

同一回环隔离负载中，控制端显示器查询人工120ms：修复前客户端RTT最高122.209ms（18次中17次>100ms），修复后0.394ms（18次均未>100ms）；慢查询实际执行120.192–125.038ms，确认不是移除负载。debug构建与mock平台，仅证明已消除该阻塞传播路径，不能外推为真实Wi-Fi全部尖峰消除。所有原始数据与临时实验源码保存在仓库外。

本地检查日志：`/tmp/screen-ferry-latency-*.log`。未替换已安装应用或改变传输设置；两平台Actions开发包在提交后生成，不创建Release或tag。

## 延迟第二批：平台调用、收发隔离与诊断（2026-11-07）

见[ADR0020](adr/0020-ordered-platform-work-and-session-io.md)。本批落实已确认的程序内部阻塞路径，不把它当作有线165ms与重传事件已经解决。

| 检查 | 结果 |
|---|---|
| Rust默认工作区 | 477通过，0失败，3忽略 |
| Rust全feature工作区 | 488通过，0失败，3忽略 |
| 默认/全feature全target Clippy | 通过 |
| Windows GNU全feature全target交叉Clippy | 通过，不是原生执行 |
| Rust/Dart格式、结构lint、git diff、cargo-deny | 通过，deny保留原有非阻断提示 |
| Flutter全套及analyze | 141通过，analyze无问题 |

新增20项行为回归：平台输入9项（慢操作心跳/顺序、取消Down/旧代租约、2秒超时停止、失败Up/reset重试、部分grab失败清理）；拖放3项（慢读取与实际放下、代次与频率边界）；回执清理4项（慢磁盘期间其他任务进展、取消与错误/panic、更新回执身份）；会话2项（接收排队计入注入、重复/未发送Pong拒绝）；Framed拆分1项（预读/半帧/写背压时独立读取）；诊断关联及累计1项。

最终源码保留输入可靠顺序和QUIC控制屏障。独立reader响应心跳意味着RTT可以低于受阻输入处理，因此UI/IPC文档明确新的测量边界；inject包含接收队列，发送指标含writer等待，不以RTT冒充键鼠到显示延迟。

三项忽略仍为两项组播及一项需要活动显示器的原生测试。未新做原生双机交互、未替换用户安装版、未改变网络/协议配置。检查日志在`/tmp/screen-ferry-phase2-*.log`；两平台原生CI及安装包结果在推送后核实。

首次CI（03e7afd）默认检查通过，两平台全feature分别在平台工作线程用例超时。定位为新增测试Rig仅等客户端初始隐藏便发送唯一触边事件，但控制端可能尚未登记邻居；该事件在正常“未连接邻居”状态下被忽略。测试改为等待控制端Connected及客户端初始化两项条件，没有改变生产逻辑、增大超时或削弱顺序断言；针对该准备阶段执行并发重复回归后重跑CI。

## 首次文件拖放无反应修复（2026-11-07）

用户明确日常Mac mini走Wi-Fi、Windows有线，直连不是长期要求。本次针对Mac→Windows或双向拖文件偶尔要重复才启动，修复了异步拖放基线晚到、临时空读取锁死本次识别、接收条与首帧交接、MouseUp早于原生Drop、旧文件卡片窗口/设备目标残留等具体路径，见ADR0021。

新增8项回归，拖放全套27通过；真实TLS测试验证第一轮放下即传到下载目录，未以重复拖动掩盖时序。只发送原生Drop给出的路径，Up和缓存都不触发发送；取消/断线/多设备目的地有负向回归。准备清单前显示反馈，阶段日志不含文件名或坐标。

最终本地默认工作区485通过、全feature496通过，各3项既有环境依赖忽略；默认/全feature/Windows GNU全target Clippy、Rust格式、14crate结构lint、git diff与cargo-deny通过。Flutter源码未改变。原生Mac透明接收条已编译和静态检查，仍需安装后原生拖放确认。未操作当前安装版或改变网络设置。日志：`/tmp/screen-ferry-drop-fix-*.log`。
