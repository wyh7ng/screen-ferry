# 剩余开发任务与完成证据

更新：2026-11-07。用户要求停止继续人工验证，先完成开发；技术问题自主决策。
工作副本：`codex/reliability-fixes`，保留首批修复，不发布、不安装覆盖原应用。

## 范围

2026-11-07延迟第二批已完成本地整合：有序平台输入线程及2秒确认期限/跨代清理租约；拖放读取隔离；回执清理锁外磁盘与保留标记；独立输入reader/有序writer；关联本机引擎窗口和累计统计。默认477/全feature488/Flutter141通过，详见ADR0020与repair-validation最新记录。真实有线残余165ms与重传的因果仍未定位，QUIC对照及高频位置过期策略不属于本批已完成项。

2026-11-07新增延迟第一批已实现并通过本地整合：显示器查询后台隔离、查询/光标/心跳阶段统计、有界尖峰上下文、应用RTT和采样年龄展示。慢查询回环回归及默认457/全feature468/Flutter141通过，详见repair-validation.md最新记录与ADR0019。Windows光标异步协调、拖拽/清理锁隔离、会话读写拆分和高频位置策略属于后续批次，不能将本批记为全部延迟优化完成。

本清单来自优化方案、两轮代码审阅及首批遗留项。条件性性能候选需要形成可运行实现与清楚的默认行为；不能用“等待用户实测”代替可以完成的开发。

| 工作 | 开发状态 | 完成证据 / 待办 |
|---|---|---|
| 首批可靠性、输入线程隔离、原生资源与IPC修复 | 已完成 | repair-validation.md；默认与全feature工作区回归通过 |
| 输入各阶段耗时与队列统计 | 已完成 | 本机单调时钟、近似P50/P95/P99、max、>100ms、队列深度与溢出；IPC/界面/诊断均接入 |
| 输入队列背压与鼠标合并 | 已完成 | 有界捕获、过载终止归还控制；可选合并布局后连续普通绝对移动，默认关闭；拖拽/按键/切屏为屏障 |
| 文件共享限速与发送节奏 | 已完成 | 所有文件任务共享预算，在线设置0..1024 MB/s，0不限；不声称隔离物理带宽 |
| 文件提交中断恢复与回执清理 | 已完成 | 持久意图/对象身份/内容核对，保留用户更改；去重窗口7天/最多1024份；平台掉电保证如实记录 |
| 日志增量读取与后台写入 | 已完成 | 文件身份+锚点+offset游标；有界分页；有界writer不阻塞输入，过载丢日志有计数 |
| 高频引擎事件合并与IPC任务管理 | 已完成 | 按设备/任务合并RTT、指标、进度；终态屏障；界面握手与请求队列有界、统一回收 |
| Flutter分域刷新与新配置/统计界面 | 已完成 | 实际build次数回归；页面不因无关RTT/下载进度重建；日志请求编号与单请求分页 |
| resident职责拆分 | 已完成 | 命令、任务监督、诊断、状态投影分文件，保持私有实现与层次 |
| 多设备剪贴板转换与内容共享 | 已完成 | 按change_count缓存稳定快照，内容共享，变更/写入失效及内存上限 |
| Framed读取缓冲优化 | 已完成 | 读游标避免每帧搬移残余字节；保持recv取消安全与逐帧发送时序 |
| QUIC实验路径 | 已完成 | ADR0017；真实回环配对/可靠流/DATAGRAM屏障、自动TCP回退与UDP重启恢复；默认关闭 |
| 文档、配置迁移、自动回归 | 已完成 | schema 6→7无行为突变；IPC仍为未发布的3；双语README和ADR、最终检查记录 |

## 已完成的人工验收（停止继续安排）

用户确认 Windows 控制 Mac：强制返回、20次往返切屏、英文/中文、快捷键/拖选/双击/滚轮、双向文本与图片、最新剪贴板保持、Windows后台重启后自动重连不再配对、Mac后台重启后输入与粘贴正常。
这覆盖首批固定CLI二进制；本轮开发改动尚未做双机验证。没有声称完整GUI/文件传输/反方向控制/更新/UAC/休眠或100ms尖峰消除。必要编译与自动检查由开发过程执行，不要求用户参与。


## 第二批阶段证据

- Flutter完整133项测试与analyze通过；实际build-count证明RTT/下载进度不重建无关设置/侧栏/页面框架。
- 文件相关38项、Mac原生提交4项、Windows交叉Clippy通过；详见repair-validation对应子范围。
- resident34项（随后追加诊断与退出/指标回归）通过，含增量日志游标换卷/半行/1000行分页、性能设置持久化。
- 网络缓冲20项通过/1项已有组播忽略，日志writer与配置迁移相关store23项通过、types26项通过（后续共享内容增加测试）。
- 共享剪贴板21项、types内容4项通过，覆盖计数变化/读取失败/远端写入/过期和64MiB预算。
- QUIC已具备可靠与DATAGRAM两模式、仅Input/Pair QUIC而Data/File TCP、自动TCP回退；独立审阅发现的多地址超时预算、高频移动ACK收敛性、边缘点屏障与连接池并发问题已修复并有回归。

以上为阶段记录；最终完成证据以本节后面的整合检查与范围核对为准。


## 完成范围核对

| 要求 | 实现来源 | 针对性证据 |
|---|---|---|
| 输入顺序、队列容量、延迟统计 | engine/input_queue.rs、metrics.rs、server.rs、client.rs；platform/capture.rs | queue7、metrics3、server capture溢出释放/按键拖拽/阶段归属回归；resident连接代次清理回归 |
| 总文件限速与取消 | engine/files/budget.rs、send.rs；Config/IPC/UI设置 | paused clock共享FIFO、在线降速、小块进展、取消回归；设置持久化与widget交互 |
| 提交中断恢复、回执有界 | engine/files/commit.rs、files.rs、platform/file_commit.rs | 旧引擎销毁后新Files只读磁盘恢复、跨卷stage、目标内容修改保护、过期与活跃任务保护 |
| 增量日志与慢盘隔离 | resident/diagnostics.rs、store/log_writer.rs、daemon/logging.rs | 不完整行/大于原卷轮转/2005行有界分页、写失败计数恢复、轮转失败保留、导出告警与64KiB摘要 |
| 高频状态与连接任务 | engine/events.rs、resident/ui.rs、resident.rs | 10000次RTT合并且不跨终态、多UI churn后全部收到Quit、任务/通道容量检查 |
| UI局部刷新与可观察配置 | app/state/aspects.dart、app_scope.dart、pages/performance_card.dart、settings_page.dart | 真实widget build-count、窄/宽布局、旧消息defaults、游标request_id迟到拒绝 |
| 剪贴板共享转换结果 | types/clipboard.rs、engine/clipboard.rs | Arc共享隔离、真实多peer读取次数、读期间变化、错误保留、写后失效、TTL与64MiB上限 |
| 网络缓冲与模块维护 | net/framed.rs、resident/runtime/{commands,tasks,settings,reporting,projection}.rs | 半帧取消与多帧顺序、resident全场景回归、Flutter短语扫描改为整个resident目录 |
| 可选QUIC开发实验 | net/quic.rs、transport.rs、engine/quic_motion.rs、proto/motion.rs、CLI、ADR0017 | 两端证书身份/配对、未认证拒绝、旧代/乱序/重复/屏障/末位置补偿、纯可靠对照、UDP黑洞TCP回退、端口drain后自动重启 |
| 迁移、CI及维护说明 | schema7、IPC3扩展、双语README、ADR0016/0017、CI | 旧schema6迁移、Rust/Dart共享fixtures、默认和全feature CI步骤、结构lint |

## 最终整合检查（2026-11-06）

- Rust默认工作区：438通过，0失败，3跳过。
- Rust全feature工作区：448通过，0失败，3跳过。
- 跳过项：两项既有局域网组播测试；一项需要唤醒macOS显示器的原生测试。屏幕睡眠已由系统信息确认，保留原断言并改显式运行，未把它记为通过。
- Flutter：133通过；analyze无问题；67个Dart文件格式检查通过。
- 全feature全target Clippy和Windows x86_64-pc-windows-gnu全feature全target交叉Clippy通过。
- Rust格式、git diff检查通过；cargo-deny advisories/bans/licenses/sources通过，存在重复依赖和未使用许可白名单提示。
- 默认Clippy、14个crate结构lint通过；macOS arm64与Windows x64 GNU release后台均构建通过；产物、来源指纹与SHA-256保存在target/development/build-info.json。

开发交付不包含发布、安装覆盖或新一轮双机验收。QUIC为默认关闭的实验实现，未承诺无线尾延迟改善。文件提交的持久性仍受文件系统语义约束，不声称任意掉电事务。原生Windows执行、完整GUI/文件拖放/休眠等验收按用户要求停止，属于交付边界而非隐藏通过项。


完成状态：本清单内开发项全部完成，改动保留在 `codex/reliability-fixes`，尚未提交、推送或发布。最新CLI开发产物在 `target/development/`，仅生成未运行。完整Flutter安装包不属于本次源码开发交付；现有Xcode/Windows原生构建环境要求仍按README，未以旧GUI混用新后台。


## 安装后追加修复

ADR0018扩展原先仅CLI的QUIC路径：Actions安装版可从设置选择输入传输，显示实际协商结果及回退原因，schema8旧配置默认TCP；已有应用不会自动切换。DMG制作移除签名后FinderInfo写入并验证最终只读映像中的应用。最新证据见repair-validation对应章节，首批与第二批的历史测试计数保留。
