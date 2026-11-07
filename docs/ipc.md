# 本机通信协议（设置界面 ⇄ 后台）

- 当前版本：**3**（版本 3 为界面命令增加结果确认，并把配对回答绑定到本次后台会话）
- 实现：`crates/ferry-ipc`（Rust，后台）、`app/lib/ipc/`（Dart，设置界面）
- 样例：`app/test/fixtures/ipc_samples.json`，由 `ferry-ipc` 的测试生成，两边的测试都按它编解码
- 决策：ADR 0004；每台设备的单独设置见 ADR 0009

本文是协议的权威说明。

## 1. 找到后台

后台启动时监听 `127.0.0.1` 的随机端口，写下 `ipc.json`：

| 系统 | 位置 |
|---|---|
| macOS | `~/Library/Caches/ScreenFerry/ipc.json` |
| Windows | `%LOCALAPPDATA%\ScreenFerry\ipc.json` |

```json
{ "port": 51234, "token": "64 位十六进制", "pid": 4242, "version": 3 }
```

文件不存在、端口连不上，都表示后台没有运行。后台每次启动都换新端口和新令牌，界面断线后应重新读取这个文件再连。用 `--data-dir` 运行的后台把 `ipc.json` 写在该数据目录里。

## 2. 消息格式

- 每行一条 UTF-8 JSON，以 `\n` 结尾，单行不超过 1 MiB。
- `type` 字段区分消息种类，取值为 snake_case。其余字段与 `type` 平级。
- 时刻一律是 Unix 毫秒（整数），`activity[].hour` 是 Unix 秒。
- 指纹有两种写法：`fingerprint` 是完整的 64 位小写十六进制，用作标识；`short` 是显示用的短形式。
- 接收方忽略不认识的字段。

## 3. 握手

1. 界面发送 `{"type":"hello","token":"…","version":3}`，必须是第一条，3 秒内发出。
2. 令牌不对、版本不同或第一条不是 `hello`，后台直接断开（第 7 节的独立 `update_health` 探测除外）。
3. 后台回 `{"type":"welcome","version":3}`，接着发送 `history`（已有日志，最新的在前）、`state`（当前状态），以及仍在等待的 `pairing`。

## 4. 后台 → 界面

| type | 字段 | 说明 |
|---|---|---|
| `welcome` | `version` | 握手成功 |
| `command_result` | `session`、`id`、`ok`、`message` | 指定命令处理结果，只回复发出命令的界面 |
| `state` | `state` | 完整状态快照，有变化时推送，最多每秒 4 次。界面整体替换，不做合并 |
| `history` | `entries` | 连接时发送一次 |
| `log` | `entry` | 新日志 |
| `pairing` | `prompt`：`session`、`id`、`peer_name`、`fingerprint`、`short`、`code` | 请用户核对配对码 |
| `pairing_closed` | `id` | 这次配对已结束，关闭对话框 |
| `toast` | `text`、`phrase` | 简短提示。`phrase` 见 4.1 节 |
| `show_device` | `fingerprint` | 翻到这台设备的设置页：它请本机打开了设置（本机是服务端、它被本机控制时） |
| `dialed` | `request_id`（可空）、`address`、`invite`、`error` | 一次 `connect_address`（`invite` 为 false）或 `invite`（为 true）的结果。`error` 为 null 表示成了：按地址连接是已经连上（没配对时接着发 `pairing`），邀请是对方答应了、正在连过来。否则是 `bad_address`（地址不对）、`refused`（那里没有在等的 ScreenFerry）、`unreachable`（连不上）、`not_screen_ferry`、`incompatible`（版本不兼容）、`busy`（对方正连着别的电脑）、`wrong_role`（本机的角色做不了） |
| `quit` | | 后台要退出了（托盘里的“退出”或 `quit` 命令），界面随之关闭。之后连接断开 |
| `run_log` | `lines`、`next`、`cursor`、`request_id`、`fresh`、`more`、`error`、`bytes`、`lines_total` | 回应 `read_run_log`，只发给发出命令的界面。`lines[]` 是 `{ at（本机时间 HH:MM:SS，旧格式为空）, level(debug\|info\|warn\|error), text }`，设备名等照原样显示；`fresh` 为真时整个替换，否则接在后面；`next` 是下次读的位置；`bytes` 是当前卷大小；`lines_total` 是当前游标累计读到的完整行数，首次尾读不扫描全卷 |
| `diagnostics_exported` | `path`、`bytes` | 诊断文件存好了，只发给发出命令的界面 |
| `diagnostics_summary` | `text` | 诊断摘要，只发给发出命令的界面，界面复制到剪贴板 |

`state` 的结构（字段含义见 `crates/ferry-ipc/src/message.rs`）：

```text
session   后台每次启动重新生成的会话标识
local     name · fingerprint · short · role(server|client) · data_dir · version · port（服务端监听的端口，否则 null）
          platform（本机系统，macos|windows…；旧版后台没有，为空）
          natural_scrolling（macOS：开着“自然滚动”；其他系统为 null）
          addresses[]（本机的局域网 IPv4 地址，常用的在前）· invite_port（客户端等邀请的端口，否则 null）
          system（本机系统和版本，例如“macOS 15.6 · arm64”）· verbose_since（详细记录从什么时候开着，Unix 毫秒；关着为 null）
sharing   用户没有暂停共享
problem   null | { kind(permission|port_in_use|other), message, port（端口被占用时是哪个端口，否则 null） }
link      status(paused|starting|waiting|searching|connecting|connected|stopped)
          peer(null | { name, fingerprint, short, addr }) · since · cursor_on_peer · rtt_ms[] · rtt_measured_at（Unix毫秒，可空）
          连着几台时是摘要：指光标所在（否则最近连上）的那台；每台是否连着见 peers
clipboard null | { to_peer, peer, chars, bytes, preview, rich（带格式）, at,
                 image（null | { width, height, path（预览用的 PNG 文件，存不下来时为 null）}） }
peers[]   { name（它自己报的名字）, fingerprint, short, online, last_seen,
            side（服务端：在本机哪一侧；客户端为 null）, cursor（光标正在它上面）,
            alias（本机给它起的名字，空为没起）, platform（macos|windows，本次运行还没看出来时为空）,
            addr（上次连上的地址，没有为空；端口是缺省值时省略）,
            version（它的 ScreenFerry 版本，本次运行还没连上过为空） }
found[]   { name, fingerprint, short, addr, paired }        只有客户端会有
settings  { name, side(left|right|top|bottom), layout[], announce, clipboard,
            clipboard_limit_mb（剪贴板单项上限，1–64）, mouse_dpi（本机鼠标的 DPI，100–12800）,
            file_rate_limit_mbps（十进制 MB/s，0–1024，0 不限）、coalesce_motion（普通移动合并，缺省false），
            server（客户端：只连这个地址，空为自动寻找）, autostart, theme(system|light|dark), accent,
            language(system|zh|en)（界面语言，system 为跟随系统）,
            auto_update（每天检查一次更新，有新版本时在后台下载好）, devices[] }
          layout[] { fingerprint, side }：单独安排过位置的设备，按先后排列，同一侧的几台按这个先后
          平分那条边（左右两侧从上到下，上下两侧从左到右）；side 是没有列出的设备放的那一侧
          devices[] 单独设置过的设备，没列出的用缺省值（括号里）：
            { fingerprint, alias（""）, pointer_dpi（0：和本机一样）, reverse_vertical（false）,
              reverse_horizontal（false）, scroll_speed(slow|normal|fast)（normal）,
              modifiers(habit|plain|custom)（habit）, text_shortcuts（false）, edge_delay_ms（0）,
              clipboard（true）, files（true）,
              custom_modifiers { gui, ctrl, alt, caps_lock }：各为 ctrl|shift|alt|gui|caps_lock|none,
              keys[] { from, to }：USB HID 扩展用法码，to 为 0 表示不发送 }
            鼠标、按键、防误切只在本机控制它（本机是服务端）时生效；剪贴板、拖文件两边都生效
activity[] { hour, switches, clips, connected_secs }         最近 48 小时，只列有活动的小时
update    { status(idle|checking|up_to_date|available|downloading|ready|installing|failed),
            latest(null | { version, published（Unix 毫秒）, size（本机平台的更新包大小）, notes_zh, notes_en,
                            breaks_peers（新版本改了线协议，连着的设备也要更新） }),
            done · total（下载进度，字节）· error(null | unreachable|verify_failed|stale|no_keys|install_failed)
            message（出错的原话）· checked_at · can_install（这份能自己装更新）
            needs_local_confirm（Windows 被控制时，安装要在本机确认 UAC）· dismissed（横幅上点过“稍后”的版本） }
          见 ADR 0014；旧版后台没有这个字段
```

### 4.1 日志条目与 phrase

`history` 和 `log` 里的日志条目是 `{ id, kind(cursor|clip|link|file), icon, text, detail, at, phrase }`。

`text`、`detail` 是后台写好的中文两行。`phrase` 是同一件事的结构化写法 `{ key, args }`（`args` 的值都是字符串，
数字、真假也写成字符串），界面按当前语言把它说出来，认不出 `key` 时显示 `text`、`detail`（ADR 0013）。
`toast` 的 `phrase` 同理，对应 `text`。

| 用在 | key | args |
|---|---|---|
| 日志 | `started` | `name`、`role`(server\|client) |
| 日志 | `cannot_share` | `problem`(permission\|port_in_use\|other)、`message`、`port`（端口被占用时） |
| 日志 | `listening` | `port`、`announced` |
| 日志 | `connected` | `name`、`ip` |
| 日志 | `disconnected` | `name`、`reason`（可选） |
| 日志 | `peer_opened_settings` | `name` |
| 日志 | `cursor_to_peer` | `name`、`side`(left\|right\|top\|bottom) |
| 日志 | `cursor_back` | `hotkey` |
| 日志 | `cursor_here`、`cursor_returned` | `name` |
| 日志 | `clipboard_synced` | `name`、`to_peer`、`bytes`、`content`(image\|rich\|text\|data)；图片另有 `width`、`height`，文字另有 `chars` |
| 日志 | `clipboard_skipped` | `name`、`to_peer`、`format`(image\|text\|rich)、`bytes`、`limit` |
| 日志、提示 | `paired` | `name`、`short` |
| 日志 | `pairing_rejected` | `name`、`by_peer` |
| 日志 | `pairing_request` | `name`、`code` |
| 日志 | `transfer_sending`、`transfer_receiving`、`transfer_sent`、`transfer_interrupted` | `name`、`file`（第一项的名字）、`items`、`files`、`bytes` |
| 日志 | `transfer_received` | 同上，另有 `folder`（保存的文件夹） |
| 日志 | `transfer_cancelled` | 同上，另有 `by_peer` |
| 日志 | `transfer_failed` | 同上，另有 `reason` |
| 日志、提示 | `sharing_resumed`、`sharing_paused` | |
| 日志 | `layout_arranged` | |
| 日志 | `layout_moved` | `name`（空为几台设备）、`side` |
| 日志 | `role_changed` | `role` |
| 日志、提示 | `removed` | `name` |
| 提示 | `peer_settings_opened`、`peer_settings_unsupported`、`files_disabled`、`sending`、`peer_offline` | `name` |
| 提示 | `error`、`autostart_failed`、`save_failed`、`peers_read_failed`、`remove_failed`、`diagnostics_failed` | `message` |
| 提示 | `diagnostics_missing`、`not_connected`、`server_gone`、`paused_connect_later`、`not_sharing_invite_later` | |
| 日志、提示 | `update_installed` | 日志 `from`、`to`；提示 `to` |
| 日志 | `update_not_installed` | `version`、`current` |
| 日志、提示 | `update_failed` | `message` |
| 提示 | `update_cancelled` | |

## 5. 界面 → 后台

| type | 字段 | 说明 |
|---|---|---|
| `hello` | `token`、`version` | 见第 3 节 |
| `execute` | `session`、`id`、`request` | 需要结果确认的命令封装，见第 7 节 |
| `set_sharing` | `on` | 暂停或继续共享 |
| `update_settings` | `settings`：`settings` 的任意子集，另可带 `role`（server\|client） | 只改出现的字段；`layout` 整份替换；改 `role` 会断开现有的连接、以新角色重新开始 |
| `set_device` | `device`：`settings.devices[]` 的一项，整份 | 换上一台设备的单独设置；全是缺省值即恢复默认 |
| `open_peer_settings` | `fingerprint` | 客户端：请连着的服务端打开它的设置界面、翻到本机的设置页，结果以 `toast` 告知 |
| `forget` | `fingerprint` | 删除一台已配对设备，连同它的单独设置 |
| `pair_reply` | `id`、`accept` | 回答配对请求，必须包含在 `execute` 中 |
| `connect` | `fingerprint` | 客户端：连接 `found` 中的一台服务端，未配对时先配对 |
| `connect_address` | `address`：IP、IP:端口或电脑名，不写端口用 24810 | 客户端：连接这个地址上的服务端，未配对时先配对，结果以 `dialed` 告知。只连这一次，不改 `settings.server`；连上后断线会回到这个地址重连 |
| `invite` | `address`：同上，不写端口用 24811 | 服务端：请这个地址上的客户端连过来，结果以 `dialed` 告知 |
| `open_permission_settings` | | 打开系统中授予键鼠控制权限的页面 |
| `clear_log` | | 清空日志 |
| `open_settings` | | 请后台打开设置界面（另一份后台用） |
| `read_run_log` | `cursor`、`request_id`；兼容旧 `since` | 首次取尾部最多500行，后续回传opaque cursor增量分页；每页最多128KiB/1000行，响应回相同request_id。见第8节 |
| `export_diagnostics` | | 把诊断文件存到“下载”文件夹（设备名、文件名、用户目录换成代号），结果以 `diagnostics_exported` 告知，出错时以 `toast` |
| `copy_diagnostics` | | 取诊断摘要（版本、系统和最近 200 行运行日志），结果以 `diagnostics_summary` 告知 |
| `reveal_diagnostics` | | 在访达或资源管理器里显示最近导出的诊断文件 |
| `open_logs_folder` | | 打开运行日志所在的文件夹 |
| `set_verbose` | `on` | 打开或关闭详细记录（DEBUG 级别）。重启后总是关着 |
| `check_update` | | 马上检查更新，结果体现在 `state.update` 里 |
| `download_update` | | 下载发现的新版本（关掉了自动检查时） |
| `cancel_update` | | 停下正在进行的下载，已下载的部分留着下次接着用 |
| `install_update` | | 安装下载好的新版本：后台退出，新版本启动后设置界面重新打开（ADR 0014） |
| `dismiss_update` | `version` | 仪表盘横幅上点了“稍后”，到下一个版本前不再提示 |
| `quit` | | 请后台退出：停下共享（归还键鼠、松开按键），给所有界面发 `quit` 后退出。`screenferry quit` 用，安装程序升级、卸载前靠它放开程序文件 |
| `send_files` | `paths`：本机绝对路径的列表；可选 `to`：目标设备的证书指纹 | 把这些文件和文件夹发给 `to`，不填时发给光标最近去过的那台（只连着一台时就是它；`screenferry send` 用），结果以 `toast` 和日志告知 |

除了 `connect_address`、`invite` 以 `dialed` 回复，`read_run_log`、`export_diagnostics`、`copy_diagnostics` 有各自的通知，
直接发送的命令结果体现在随后的 `state`、`log`、`toast` 里；界面使用 `execute` 为设置修改、设备移除、配对回答和连接邀请取得逐条回执（第 7 节）。

## 6. 修改协议

1. 改 `crates/ferry-ipc/src/message.rs` 与本文。
2. 运行 `UPDATE_IPC_SAMPLES=1 cargo test -p ferry-ipc` 重新生成样例。
3. 改 `app/lib/ipc/` 中对应的 Dart 类型，直到 `flutter test` 通过。
4. 只加字段、加消息种类不改版本号；改变已有字段的含义或删除字段时递增 `VERSION`。


## 7. 命令确认、启动会话和更新探测

设置界面与后台一起更新到协议版本 3。`state.session` 和 `pairing.prompt.session` 是每次后台启动重新生成的随机会话标识。
界面断线立即清空待确认命令及配对弹窗；收到新的完整状态后才能发命令。配对回答必须使用当前弹窗的会话标识，禁止直接发送未封装的 `pair_reply`。

需要确认结果的命令写成 `{"type":"execute","session":"…","id":42,"request":{"type":"update_settings","settings":{"accent":1}}}`。
`id` 由每个界面递增生成。后台处理完成后先向该界面发送最新 `state`，再发送 `{"type":"command_result","session":"…","id":42,"ok":true,"message":""}`；失败为 `ok:false`，界面仅撤销该编号的乐观更新。未确认的界面修改三秒后回退。
`dialed` 新增可空 `request_id`，关联 `execute.id`；连接/邀请的异步结果不会覆盖较新的操作。

更新后的健康探测独立于 UI 协议版本：首条消息 `{"type":"update_health","token":"…"}`，后台只核对本次 `ipc.json` 的令牌，回复 `{"type":"update_health","version":"0.2.0"}` 后关闭连接。
旧进程核对新进程返回的实际应用版本，不使用普通 `hello` 的协议版本作为更新成功条件。


## 8. 性能与增量日志扩展

未发布的IPC3追加可选字段，旧消息缺少时取默认值。`settings`/`update_settings.settings`支持`file_rate_limit_mbps`和`coalesce_motion`，设置持久化失败时保留原值并返回命令失败。

`state.performance.peers[]`为`{ fingerprint: string|null, measured_at: Unix毫秒, metrics }`，null指本机引擎公共阶段（控制端捕获/光标切换/显示器查询，以及被控端启动查询），有fingerprint也仍是本机对该连接的统计，不是对端测量。`metrics.window_ms`给出采样窗口；`capture_queue/send_queue/send/inject/rtt/scheduler/display_query/cursor_switch/heartbeat_send`均为`{count,p50_us,p95_us,p99_us,max_us,over_100ms}`，新增阶段缺省全零，单位微秒，分位数是固定直方图近似，count=0不解释为0ms。`capture/outbound`为`{depth,high_water,oldest_us,max_wait_us,coalesced,overflows}`；oldest_us为null表示不能测量当前最老事件，max_wait_us是该窗已消费事件最大等待。仅本机时钟差值，不是跨机单程时间。

`display_query`测后台线程内显示器API执行，慢查询不代表核心线程被阻塞；`cursor_switch`测控制端grab/release和被控端显示/隐藏的确认等待。`send`和`heartbeat_send`测消息进入有序写队列至编码、写入及flush的总等待（包括失败），不代表对端确认。`inject`从reader解码输入起计，包含接收队列及平台线程等待，止于原生API返回，不含呈现。`scheduler`是该任务检查定时器的迟到量，包含异步等待，不自动表示整个运行时CPU阻塞。

`rtt`由独立reader处理匹配Pong后计时，包含协议发送队列、网络及对端心跳响应，不是纯网络RTT或输入到显示时间。reader在平台输入等待时仍可回应Ping，因此低RTT不保证低输入延迟；需要同时看inject/cursor_switch。重复及未发nonce的Pong不产生RTT样本。

`link.rtt_ms`最多保留32个有样本的结算窗口均值，空窗口不插值；未消费RTT事件可合并，总览不是无损全样本统计。`rtt_measured_at`是最新已结算平均中最后一个样本被resident观测的时间，不是报文到达内核的时间；旧消息缺省null，重连/断线清空。历史峰值是这组窗口均值的最大值，不代表当前延迟。

完整诊断文件新增有界窗口上下文：常规60秒、每来源12窗、全局128窗；另存最近4次>100ms事件，每次最多前2/触发1/后2个同源窗口，重叠触发不延长容量。`related_local_sources`关联触发前后15秒的本机公共阶段（连接触发）或连接阶段（引擎触发），每次最多16窗，其中历史最多8窗给后续留空间；多连接仍受总容量限制。`totals`保留本来源生命周期已收到窗口的count/max/over_100ms，最多128来源，详细事件淘汰不丢较早累计值；事件合并可能漏窗口，不能视为全量无损追踪。

来源断线/重连及引擎重启清空对应记录及关联内容。仅保存在后台内存，完整导出时包含JSON；复制摘要只含事件数量，不向每次State附加历史，不记录键值/文字/位置。各阶段都可触发上下文，后台查询慢本身不表示输入卡顿；时间相邻的不同阶段不能直接相加减为一次操作的分段。

`read_run_log`新增`cursor:string|null`、`request_id:u64|null`，保留`since`兼容。新界面优先原样回传opaque cursor，不自行解析；后端回相同request_id。`run_log.cursor`用于下一页，`more=true`表示当前还有可读取的分页；`fresh=true`替换旧行。`error`非空时保留旧行与游标，释放在途状态稍后重试；界面拒绝不匹配当前request_id的迟到响应。换页/断线时取消观察不会让旧响应覆盖新一轮。


## 9. 安装版输入传输

配置schema8：`settings.input_transport`及patch的同名可选字段取`tcp|auto|quic|quic_reliable`，旧配置缺省tcp。修改协议重启输入引擎，并通过原Execute/CommandResult确认或回滚。
`local.quic_available`表示本构建是否包含QUIC，缺省false；无此能力设置非tcp会明确失败，auto也不静默冒充可用。
`link.input_transport`及`peers[].input_transport`为实际输入连接协议`tcp|quic|quic_reliable`或null；null表示未建立/旧后台未知。`fallback_reason`为null或稳定代码`quic_unreachable`，只在实际回退的客户端可确认该原因；服务端不猜对端偏好。断线清空实际状态，偏好仍保留。QUIC是允许普通位置数据报，按键/拖拽/最终位置补偿仍可靠，数据和文件始终TCP。新日志phrase `input_transport`参数为name/transport/fallback_reason。
