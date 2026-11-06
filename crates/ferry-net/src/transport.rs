//! 连接选择与统一字节流。TCP为默认；QUIC只由显式实验入口启用。

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// `Auto`只在连接建立失败时回退TCP，不重发已建立会话中的输入。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    #[default]
    Tcp,
    Quic,
    Auto,
    /// 对照实验：QUIC可靠流，不协商位置DATAGRAM。
    QuicReliable,
    AutoReliable,
}

impl From<ferry_types::InputTransport> for Transport {
    fn from(value: ferry_types::InputTransport) -> Self {
        match value {
            ferry_types::InputTransport::Tcp => Self::Tcp,
            ferry_types::InputTransport::Auto => Self::Auto,
            ferry_types::InputTransport::Quic => Self::Quic,
            ferry_types::InputTransport::QuicReliable => Self::QuicReliable,
        }
    }
}

impl Transport {
    pub const fn quic_available() -> bool {
        cfg!(feature = "quic-experiment")
    }
    pub fn ensure_available(self) -> io::Result<()> {
        if self != Self::Tcp && !Self::quic_available() {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "此版本未包含QUIC，请安装支持QUIC的版本或选择TCP",
            ))
        } else {
            Ok(())
        }
    }

    pub fn falls_back(self) -> bool {
        matches!(self, Self::Auto | Self::AutoReliable)
    }
    pub fn datagrams(self) -> bool {
        matches!(self, Self::Quic | Self::Auto)
    }
    pub fn reliable_only(self) -> Self {
        match self {
            Self::Quic => Self::QuicReliable,
            Self::Auto => Self::AutoReliable,
            other => other,
        }
    }
    /// Auto先等QUIC握手，再留完整TCP时间；外层不能用同一个2秒预算吞掉fallback。
    pub fn connect_budget(self, tcp_budget: std::time::Duration) -> std::time::Duration {
        if self.falls_back() {
            tcp_budget + std::time::Duration::from_secs(2)
        } else {
            tcp_budget
        }
    }
}

pub enum TransportStream {
    Tcp {
        stream: Box<tokio_rustls::TlsStream<TcpStream>>,
        fallback_reason: Option<String>,
    },
    #[cfg(feature = "quic-experiment")]
    Quic(Box<crate::quic::QuicStream>),
}

impl TransportStream {
    /// 稳定原因码，不包含IP、证书、用户名等内容。
    pub fn fallback_reason(&self) -> Option<&str> {
        match self {
            Self::Tcp {
                fallback_reason, ..
            } => fallback_reason.as_deref(),
            #[cfg(feature = "quic-experiment")]
            Self::Quic(_) => None,
        }
    }
    pub(crate) fn set_fallback_reason(&mut self, reason: &'static str) {
        match self {
            Self::Tcp {
                fallback_reason, ..
            } => *fallback_reason = Some(reason.into()),
            #[cfg(feature = "quic-experiment")]
            Self::Quic(_) => {}
        }
    }
    pub fn is_quic(&self) -> bool {
        match self {
            Self::Tcp { .. } => false,
            #[cfg(feature = "quic-experiment")]
            Self::Quic(_) => true,
        }
    }

    pub fn datagrams(&self) -> Option<Datagrams> {
        match self {
            Self::Tcp { .. } => None,
            #[cfg(feature = "quic-experiment")]
            Self::Quic(stream) => Some(Datagrams(stream.connection.clone())),
        }
    }
}

impl AsyncRead for TransportStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp { stream, .. } => Pin::new(stream.as_mut()).poll_read(cx, buf),
            #[cfg(feature = "quic-experiment")]
            Self::Quic(stream) => Pin::new(&mut stream.recv).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TransportStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp { stream, .. } => Pin::new(stream.as_mut()).poll_write(cx, buf),
            #[cfg(feature = "quic-experiment")]
            Self::Quic(stream) => AsyncWrite::poll_write(Pin::new(&mut stream.send), cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp { stream, .. } => Pin::new(stream.as_mut()).poll_flush(cx),
            #[cfg(feature = "quic-experiment")]
            Self::Quic(stream) => AsyncWrite::poll_flush(Pin::new(&mut stream.send), cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp { stream, .. } => Pin::new(stream.as_mut()).poll_shutdown(cx),
            #[cfg(feature = "quic-experiment")]
            Self::Quic(stream) => AsyncWrite::poll_shutdown(Pin::new(&mut stream.send), cx),
        }
    }
}

/// QUIC不可靠消息接口。未编译实验feature时不能创建；不暴露第三方类型给引擎。
#[derive(Clone)]
pub struct Datagrams(#[cfg(feature = "quic-experiment")] quinn::Connection);

impl Datagrams {
    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        #[cfg(feature = "quic-experiment")]
        {
            self.0
                .send_datagram(bytes.to_vec().into())
                .map_err(io::Error::other)
        }
        #[cfg(not(feature = "quic-experiment"))]
        {
            let _ = bytes;
            Err(io::Error::other("未编译QUIC实验"))
        }
    }
    pub async fn recv(&self) -> io::Result<Vec<u8>> {
        #[cfg(feature = "quic-experiment")]
        {
            self.0
                .read_datagram()
                .await
                .map(|b| b.to_vec())
                .map_err(io::Error::other)
        }
        #[cfg(not(feature = "quic-experiment"))]
        {
            std::future::pending().await
        }
    }
}
