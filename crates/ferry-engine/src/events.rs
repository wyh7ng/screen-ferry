//! 引擎对外报告的事件：连接与断开、往返延迟、光标进出、剪贴板同步、配对结果、文件传输。
//!
//! 常驻后台用它们拼出给设置界面看的状态；命令行模式不关心，传 [`Events::none`]。

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferry_core::clip_format::png_size;
use ferry_proto::RejectReason;
use ferry_trust::Fingerprint;
use ferry_types::{ClipboardContent, ClipboardFormat, TransferId};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::error::Error;

/// 一件发生了的事。
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// 常驻后台的引擎代次。旧引擎排队中的事件不能覆盖新引擎状态。
    Engine { generation: u64, event: Box<Event> },
    /// 服务端开始等待连接。
    Listening { port: u16, announced: bool },
    /// 客户端开始在局域网里寻找服务端。
    Searching,
    /// 客户端在局域网里看到的服务端变了。
    Found(Vec<FoundServer>),
    /// 客户端正在连接。
    Connecting { addr: SocketAddr },
    /// 与对方建立了会话。
    Connected {
        name: String,
        fingerprint: Fingerprint,
        addr: SocketAddr,
        /// 对方是什么系统（`macos`、`windows`）；看不出来时为 `None`。
        platform: Option<String>,
    },
    /// 与 `peer` 的会话结束。`reason` 为 `None` 表示对方正常断开。
    Disconnected {
        peer: Fingerprint,
        reason: Option<String>,
    },
    /// 本机输入性能快照；None是控制端捕获队列，Some是该设备会话的发送/接收。
    Metrics {
        peer: Option<Fingerprint>,
        metrics: Box<crate::metrics::InputMetrics>,
    },
    /// 与 `peer` 的一次往返延迟。
    Rtt { peer: Fingerprint, rtt: Duration },
    /// 光标是否在本机。不在时它在 `peer` 上；回到本机时 `peer` 是它刚离开的那台。
    /// `hotkey` 表示是按快捷键回来的。
    Cursor {
        here: bool,
        hotkey: bool,
        peer: Fingerprint,
    },
    /// 与 `peer` 的剪贴板同步完成。`to_peer` 为真表示对方拉走了本机的内容。
    Clipboard {
        peer: Fingerprint,
        to_peer: bool,
        summary: ClipSummary,
    },
    /// 剪贴板里有一种格式超过了大小上限 `limit`，没有和 `peer` 同步。
    /// `to_peer` 为真表示是本机的内容没有发出去，否则是对方的内容没有拉过来。
    ClipboardSkipped {
        peer: Fingerprint,
        to_peer: bool,
        format: ClipboardFormat,
        bytes: u64,
        limit: u64,
    },
    /// 配对成功，已记住对方。
    Paired {
        name: String,
        fingerprint: Fingerprint,
    },
    /// 配对没有成功。`by_peer` 表示是对方拒绝的。
    PairingRejected { name: String, by_peer: bool },
    /// 一次文件传输有了进展。传输中最多每 0.1 秒报告一次。
    Transfer(TransferUpdate),
    /// 服务端：`peer` 请本机打开设置界面，翻到它的设置页。
    OpenSettings { peer: Fingerprint },
    /// 对方告诉了它的程序版本（握手后一次）。
    PeerVersion { peer: Fingerprint, version: String },
    /// 客户端：请 `peer` 打开它的设置界面的结果。`sent` 为假表示对方的版本不支持。
    PeerSettings { peer: Fingerprint, sent: bool },
    /// 客户端开始在 `port` 上等服务端的邀请。
    InviteListening { port: u16 },
    /// 用户按地址连接（客户端）或邀请一台电脑（服务端）的结果。`error` 为 `None` 表示成了：
    /// 连上了、开始核对配对码，或者对方接受了邀请、正在连过来。
    Dialed {
        /// 常驻后台分配的异步操作号，用来关联发起界面和请求；自动连接为 None。
        request_id: Option<u64>,
        /// 连的是哪里，给用户看。
        address: String,
        invite: bool,
        error: Option<DialError>,
    },
}

/// 按地址连接、邀请为什么没成。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialError {
    /// 地址写得不对，或者电脑名找不到。
    BadAddress,
    /// 那个地址上没有程序在等：没开 ScreenFerry、角色不对，或者版本太旧不支持邀请。
    Refused,
    /// 连不上：网络不通、地址不对、被防火墙挡住，或者超时。
    Unreachable,
    /// 那个地址上不是 ScreenFerry。
    NotScreenFerry,
    /// 对方的 ScreenFerry 版本与本机不兼容。
    Incompatible,
    /// 邀请时对方正连着另一台电脑。
    Busy,
}

impl DialError {
    /// 把连接时的错误归到用户看得懂的几类。
    pub fn of(error: &Error) -> Self {
        use std::io::ErrorKind;

        let io_kind = |e: &std::io::Error| match e.kind() {
            ErrorKind::ConnectionRefused => Self::Refused,
            // TLS 握手读到的不是 TLS：那个端口上是别的程序。
            ErrorKind::InvalidData => Self::NotScreenFerry,
            _ => Self::Unreachable,
        };
        match error {
            Error::Io(e) => io_kind(e),
            Error::Net(ferry_net::Error::Io(e)) => io_kind(e),
            Error::Net(ferry_net::Error::Rejected(r)) => match r.reason {
                RejectReason::INCOMPATIBLE_VERSION => Self::Incompatible,
                RejectReason::BUSY => Self::Busy,
                _ => Self::Refused,
            },
            Error::Net(
                ferry_net::Error::WrongProtocol
                | ferry_net::Error::NoPeerCertificate
                | ferry_net::Error::Decode(_)
                | ferry_net::Error::TlsConfig(_)
                | ferry_net::Error::UnexpectedMessage { .. },
            )
            | Error::Protocol(_) => Self::NotScreenFerry,
            _ => Self::Unreachable,
        }
    }
}

/// 一次文件传输的现状。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferUpdate {
    pub id: TransferId,
    /// 对方。
    pub peer: Fingerprint,
    /// 本机是发送方。
    pub outgoing: bool,
    /// 显示用的名字：只有一项时是它的名字，多项时是第一项的名字。
    pub name: String,
    /// 顶层项目数。
    pub items: u32,
    /// 全部文件数，不含文件夹。
    pub files: u32,
    /// 全部文件的总字节数。
    pub bytes: u64,
    /// 已经传过去（或已经收到）的字节数，续传时包括上次已传的部分。
    pub done: u64,
    pub state: TransferState,
}

/// 文件传输处在哪一步。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferState {
    /// 已通告，等对方接受、等文件连接打开。
    Waiting,
    Running,
    /// 完成。接收方附上放好的顶层项目；发送方为空。
    Done {
        saved: Vec<PathBuf>,
    },
    /// 连接断了。发送方会在下次连上时接着传，接收方保留已收到的部分。
    Interrupted,
    /// 被取消。`by_peer` 表示是对方取消的。
    Cancelled {
        by_peer: bool,
    },
    /// 出错，不会再继续。
    Failed {
        reason: String,
    },
}

/// 局域网里发现的一台服务端。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundServer {
    pub name: String,
    pub fingerprint: Fingerprint,
    pub addrs: Vec<SocketAddr>,
}

/// 一份剪贴板内容的摘要，用于显示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipSummary {
    /// 有文字时的字符数。
    pub chars: Option<u32>,
    pub bytes: u64,
    /// 文字的开头一段，换行替换为空格。
    pub preview: String,
    /// 带格式：有 HTML 或 RTF。
    pub rich: bool,
    /// 有图片时的那张图片。
    pub image: Option<ClipImage>,
}

/// 剪贴板里的一张图片。
#[derive(Clone, PartialEq, Eq)]
pub struct ClipImage {
    pub width: u32,
    pub height: u32,
    /// PNG 数据。常驻后台存一份到磁盘给设置界面预览，之后就丢掉。
    pub png: Arc<[u8]>,
}

impl fmt::Debug for ClipImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ClipImage({}×{}, {} 字节)",
            self.width,
            self.height,
            self.png.len()
        )
    }
}

impl ClipSummary {
    /// 预览最多保留的字符数。
    const PREVIEW_CHARS: usize = 80;

    pub fn of(content: &ClipboardContent) -> Self {
        let text = content.as_text();
        let image = content.get(ClipboardFormat::PNG).and_then(|png| {
            let (width, height) = png_size(png)?;
            Some(ClipImage {
                width,
                height,
                png: content.get_shared(ClipboardFormat::PNG)?,
            })
        });
        Self {
            rich: content.get(ClipboardFormat::HTML).is_some()
                || content.get(ClipboardFormat::RTF).is_some(),
            image,
            chars: text.map(|t| u32::try_from(t.chars().count()).unwrap_or(u32::MAX)),
            bytes: content.total_len() as u64,
            preview: text
                .map(|t| {
                    t.chars()
                        .take(Self::PREVIEW_CHARS)
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect::<String>()
                        .trim()
                        .to_owned()
                })
                .unwrap_or_default(),
        }
    }
}

/// 高频状态只保留未消费的最新值；配对、终态与命令结果按序可靠交付。
#[derive(Clone, Debug, Default)]
pub struct Events(Option<Arc<EventSink>>, Option<u64>);

type EventKey = (Option<u64>, String);
type LatestEvents = Arc<Mutex<HashMap<EventKey, (u64, Event)>>>;

#[derive(Debug)]
struct EventSink {
    sender: UnboundedSender<QueuedEvent>,
    next: AtomicU64,
    latest: LatestEvents,
}

#[derive(Debug)]
enum QueuedEvent {
    Direct(Box<Event>),
    Latest(EventKey, u64),
}

/// 与tokio接收端一样，recv可安全取消；高频事件不会随消费者暂停无限积压。
pub struct EventReceiver {
    receiver: UnboundedReceiver<QueuedEvent>,
    latest: LatestEvents,
}

impl EventReceiver {
    pub async fn recv(&mut self) -> Option<Event> {
        loop {
            let queued = self.receiver.recv().await?;
            if let Some(event) = self.resolve(queued) {
                return Some(event);
            }
        }
    }
    pub fn try_recv(&mut self) -> Result<Event, mpsc::error::TryRecvError> {
        loop {
            let queued = self.receiver.try_recv()?;
            if let Some(event) = self.resolve(queued) {
                return Ok(event);
            }
        }
    }
    fn resolve(&self, queued: QueuedEvent) -> Option<Event> {
        match queued {
            QueuedEvent::Direct(event) => Some(*event),
            QueuedEvent::Latest(key, token) => {
                let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
                if latest
                    .get(&key)
                    .is_some_and(|(current, _)| *current == token)
                {
                    latest.remove(&key).map(|(_, event)| event)
                } else {
                    None
                }
            }
        }
    }
}

impl Events {
    pub fn none() -> Self {
        Self(None, None)
    }

    pub fn channel() -> (Self, EventReceiver) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let latest = Arc::new(Mutex::new(HashMap::new()));
        (
            Self(
                Some(Arc::new(EventSink {
                    sender,
                    next: AtomicU64::new(0),
                    latest: latest.clone(),
                })),
                None,
            ),
            EventReceiver { receiver, latest },
        )
    }

    pub(crate) fn scoped(&self, generation: u64) -> Self {
        Self(self.0.clone(), Some(generation))
    }

    pub fn emit(&self, mut event: Event) {
        let Some(sink) = &self.0 else {
            return;
        };
        if sink.sender.is_closed() {
            return;
        }
        if let Event::Metrics { metrics, .. } = &mut event {
            metrics
                .collected_at
                .get_or_insert_with(std::time::Instant::now);
        }
        let key = match &event {
            Event::Rtt { peer, .. } => Some(format!("rtt:{}", peer.to_hex())),
            Event::Metrics { peer, .. } => Some(format!(
                "metrics:{}",
                peer.map(|p| p.to_hex()).unwrap_or_default()
            )),
            Event::Transfer(t) if t.state == TransferState::Running => {
                Some(format!("transfer:{}:{:?}", t.peer.to_hex(), t.id))
            }
            _ => None,
        };
        let wrap = |event| match self.1 {
            Some(generation) => Event::Engine {
                generation,
                event: Box::new(event),
            },
            None => event,
        };
        let mut latest = sink.latest.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(key) = key {
            let key = (self.1, key);
            if let Some(current) = latest.get_mut(&key) {
                current.1 = wrap(event);
            } else if latest.len() < 256 {
                let token = sink.next.fetch_add(1, Ordering::Relaxed);
                latest.insert(key.clone(), (token, wrap(event)));
                if sink
                    .sender
                    .send(QueuedEvent::Latest(key.clone(), token))
                    .is_err()
                {
                    latest.remove(&key);
                }
            }
        } else {
            // 终态/连接变化是屏障：先交付同代已合并的进度，之后不能出现旧Running覆盖Done。
            let keys: Vec<_> = latest
                .keys()
                .filter(|(generation, _)| *generation == self.1)
                .cloned()
                .collect();
            for key in keys {
                if let Some((_, pending)) = latest.remove(&key) {
                    let _ = sink.sender.send(QueuedEvent::Direct(Box::new(pending)));
                }
            }
            let _ = sink.sender.send(QueuedEvent::Direct(Box::new(wrap(event))));
        }
    }
}
