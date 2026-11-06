# ADR 0017：显式 QUIC 输入实验

状态：实验功能，默认关闭。日期：2026-11-06。

后续扩展：ADR0018将此路径接入带feature的安装版设置界面；以下CLI实验设计仍有效，常驻仅TCP的原范围已被该决策扩展。

## 范围与选择

正常构建及常驻后台继续使用 TCP + TLS 1.3。`quic-experiment` Cargo feature 只给调试
`serve/connect --transport quic|auto` 增加 QUIC 选择；`tcp` 仍是默认值。

`auto` 只在 QUIC 建立失败时尝试 TCP；已经建立的输入会话不把输入重新发送到另一条连接。
实验服务端在同一数值端口同时保留 TCP 和 UDP 监听。网络故障或缺少 UDP 不要求关闭加密。

实现采用 quinn 的可靠双向流承载原来的 Framed 消息。输入与配对流使用 QUIC 可靠流；剪贴板与每次文件传输保持 TCP。
同一远端的配对/输入流可复用一个 QUIC 连接；数据和文件连接固定走原有 TCP，避免大文件共享输入实验的拥塞窗口。客户端连接缓存和服务端连接任务均有上限，
监听器停止时关闭连接并取消其子任务。QUIC关闭的draining阶段可能短暂保留UDP端口；
引擎重启会对AddrInUse异步重试，最多3秒，可随引擎取消。非占用错误立即返回。
实验服务端也拒绝在QUIC流上声明Data/File/Invite，避免仅靠客户端约定保持数据通道为TCP。关闭单个流不提前关闭整个连接，防止丢失最后一个回执。

QUIC TLS 配置复用设备证书和签名验证器；应用握手继续核对设备证书指纹并完成六位配对。
QUIC 使用独立 ALPN `screenferry-quic-input-v1`，禁止 0-RTT，禁用客户端 TLS 会话恢复，
每次新连接仍验证设备身份。不会在配对或输入会话握手完成之前注入 DATAGRAM。

## 位置 DATAGRAM 的顺序

只有本机已经完成布局与边缘计算、无键/鼠标按钮按住、远离屏幕边缘的普通绝对位置可以用 DATAGRAM。
Key/Button/Wheel、拖拽、Enter/Leave、布局通知和其他控制消息始终走可靠流。

双方必须在 QUIC ALPN 上协商 `QUIC_MOTION`（能力位 7）。TCP 从不提供这个能力，旧端不会收到新消息。
线协议版本仍为 1；既有消息的字节布局保持原样。新可靠消息类型只用于已协商实验会话：

- `0x0130 MotionFence`：`generation u64, sequence u64, has_position bool, position? Point`。
- `0x0131 MotionAck`：`generation u64, sequence u64`。
- `0x0132 MotionSync`：`generation u64, sequence u64, barrier u64, position Point`。

DATAGRAM 固定 36 字节：`SFQ1` 魔数、generation、sequence、barrier（三者 u64 小端）、x/y（i32 小端）。
它只表示绝对位置，不允许编码任何按键或拖拽。

发送端每条输出共享递增逻辑序号；generation 使用本次已认证输入会话号。发送任何可靠控制之前，
先发 MotionFence，附尚未补偿的最新位置。接收端先应用该位置，然后处理紧跟着的控制，最后才回 ACK。
发送端收到当前 Fence 的 ACK 之前继续可靠发送移动，不阻塞键盘或控制消息，也不发送可能跨越控制的 DATAGRAM。
等待ACK期间的普通位置用MotionSync可靠发送而不增加控制屏障，保证持续移动不会使ACK永远落后。
ACK 超过 2 秒未到，该输入会话永久退回可靠移动，不无限等待。

接收端只接受当前 generation、当前已处理屏障且序号更新的 DATAGRAM；重复、乱序、旧屏障、
旧会话以及 Enter 之前/Leave 之后的包均忽略。第一次未补偿 DATAGRAM 启动固定 20ms 截止时间，
后续移动只更新最终位置、不推迟截止。到期经可靠 MotionSync 补偿最后位置；控制消息到来则立即在 Fence 中补偿。
DATAGRAM 发送失败时禁用该会话不可靠移动，并立即可靠补偿。

## 开发入口与兼容

```text
cargo build -p ferry-daemon --features quic-experiment
screenferry serve --transport quic --listen 0.0.0.0:24810
screenferry connect SERVER_IP:24810 --transport auto --file-rate-limit-mbps 20

# 纯可靠QUIC对照：两端均加该参数
screenferry serve --transport quic --quic-reliable-only --listen 0.0.0.0:24810
screenferry connect SERVER_IP:24810 --transport quic --quic-reliable-only

# TCP对照
screenferry serve --transport tcp --listen 0.0.0.0:24810
screenferry connect SERVER_IP:24810 --transport tcp
```

`--quic-reliable-only` 不协商 QUIC_MOTION，即使对端提供此能力也不发送 DATAGRAM，可与 TCP/QUIC+DATAGRAM对照。
`--coalesce-motion` 默认关闭；只合并发送队列中已积压的相邻普通移动，不增加等批次的计时器。
`--file-rate-limit-mbps` 单位为十进制 MB/s（0–1024），默认 0（不限），只影响文件发送。实验 CLI 仍绕过 config.toml，不代表常驻 GUI 配置。

## 验证边界

自动验证包括真实回环 QUIC/TLS 双端证书身份、可靠帧、配对/输入/Leave释放、UDP无监听时回退TCP，
以及可控状态机的丢包、乱序、重复、跨屏障、旧会话、最终补偿定时和 ACK 超时。

这是一条可运行、可回退的开发实验路径，没有测得真实局域网收益；不承诺较 TCP 更低的延迟。
原生双机、无线拥塞、系统调度与显示延迟不从回环或模拟指标推断。默认传输不会自动迁移。
