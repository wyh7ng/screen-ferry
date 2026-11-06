//! 显式QUIC实验：双向TLS设备证书认证、独立ALPN、可靠双向流；从不尝试0-RTT。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ferry_proto::Channel;
use ferry_trust::Fingerprint;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rustls::pki_types::CertificateDer;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;

use crate::{Framed, Result, TransportStream};

const ALPN: &[u8] = b"screenferry-quic-input-v1";
pub(crate) const CONNECT_WAIT: Duration = Duration::from_secs(2);
const MAX_CONNECTIONS: usize = 64;

type Accepted = (Framed<TransportStream>, Fingerprint, SocketAddr);

pub struct QuicStream {
    pub(crate) send: SendStream,
    pub(crate) recv: RecvStream,
    pub(crate) connection: Connection,
    _endpoint: Endpoint,
}

#[derive(Clone)]
pub(crate) struct QuicConfig {
    server: quinn::ServerConfig,
    client: quinn::ClientConfig,
    pool: Arc<Mutex<Pool>>,
}

#[derive(Default)]
struct Pool {
    endpoints: HashMap<bool, Endpoint>,
    connections: HashMap<SocketAddr, Connection>,
    gates: HashMap<SocketAddr, Arc<Mutex<()>>>,
}

impl QuicConfig {
    pub fn new(mut server: rustls::ServerConfig, mut client: rustls::ClientConfig) -> Result<Self> {
        server.alpn_protocols = vec![ALPN.to_vec()];
        server.max_early_data_size = 0;
        client.alpn_protocols = vec![ALPN.to_vec()];
        client.enable_early_data = false;
        // 每次连接都验证设备证书，不用TLS会话恢复省略证书交换。
        client.resumption = rustls::client::Resumption::disabled();
        let server_crypto =
            quinn::crypto::rustls::QuicServerConfig::try_from(server).map_err(io)?;
        let client_crypto =
            quinn::crypto::rustls::QuicClientConfig::try_from(client).map_err(io)?;
        let mut transport = quinn::TransportConfig::default();
        transport.max_concurrent_bidi_streams(32u32.into());
        transport.max_concurrent_uni_streams(0u32.into());
        transport.datagram_receive_buffer_size(Some(64 * 1024));
        transport.datagram_send_buffer_size(64 * 1024);
        transport.max_idle_timeout(Some(Duration::from_secs(15).try_into().map_err(io)?));
        let transport = Arc::new(transport);
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
        server.transport_config(transport.clone());
        let mut client = quinn::ClientConfig::new(Arc::new(client_crypto));
        client.transport_config(transport);
        Ok(Self {
            server,
            client,
            pool: Arc::new(Mutex::new(Pool::default())),
        })
    }

    pub async fn connect(
        &self,
        addr: SocketAddr,
        channel: Channel,
    ) -> Result<(Framed<TransportStream>, Fingerprint)> {
        // 每个远端单独串行建连；慢远端不能拿着全局锁堵住其他设备。
        let (endpoint, gate) = {
            let mut pool = self.pool.lock().await;
            pool.connections
                .retain(|_, connection| connection.close_reason().is_none());
            pool.gates.retain(|_, gate| Arc::strong_count(gate) > 1);
            if pool.gates.len() >= MAX_CONNECTIONS && !pool.gates.contains_key(&addr) {
                return Err(io("QUIC并发连接缓存已满").into());
            }
            let v6 = addr.is_ipv6();
            if let std::collections::hash_map::Entry::Vacant(entry) = pool.endpoints.entry(v6) {
                let ip = if v6 {
                    IpAddr::V6(Ipv6Addr::UNSPECIFIED)
                } else {
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                };
                let mut endpoint = Endpoint::client(SocketAddr::new(ip, 0))?;
                endpoint.set_default_client_config(self.client.clone());
                entry.insert(endpoint);
            }
            let endpoint = pool.endpoints[&v6].clone();
            let gate = pool.gates.entry(addr).or_default().clone();
            (endpoint, gate)
        };
        let _guard = gate.lock().await;
        let existing = self
            .pool
            .lock()
            .await
            .connections
            .get(&addr)
            .cloned()
            .filter(|connection| connection.close_reason().is_none());
        let connection = match existing {
            Some(connection) => connection,
            None => {
                let connecting = endpoint.connect(addr, "screenferry.local").map_err(io)?;
                let connection = tokio::time::timeout(CONNECT_WAIT, connecting)
                    .await
                    .map_err(|_| io("QUIC连接超时"))?
                    .map_err(io)?;
                peer(&connection)?;
                let mut pool = self.pool.lock().await;
                if pool.connections.len() >= MAX_CONNECTIONS {
                    connection.close(1u32.into(), b"capacity");
                    return Err(io("QUIC连接缓存已满").into());
                }
                pool.connections.insert(addr, connection.clone());
                connection
            }
        };
        let fingerprint = peer(&connection)?;
        let (send, recv) = connection.open_bi().await.map_err(io)?;
        Ok((
            Framed::new(
                TransportStream::Quic(Box::new(QuicStream {
                    send,
                    recv,
                    connection,
                    _endpoint: endpoint,
                })),
                channel,
            ),
            fingerprint,
        ))
    }

    pub fn listen(&self, addr: SocketAddr) -> Result<QuicListener> {
        let endpoint = Endpoint::server(self.server.clone(), addr)?;
        let (streams, ready) = mpsc::channel(32);
        Ok(QuicListener {
            endpoint,
            streams,
            ready,
            connections: JoinSet::new(),
        })
    }
}

/// 监听器持有所有连接任务；丢弃后取消握手/接收流，不留下脱离引擎的后台任务。
pub struct QuicListener {
    endpoint: Endpoint,
    streams: mpsc::Sender<Accepted>,
    ready: mpsc::Receiver<Accepted>,
    connections: JoinSet<()>,
}

impl Drop for QuicListener {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"listener stopped");
    }
}

impl QuicListener {
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    pub async fn accept(&mut self) -> Result<Accepted> {
        loop {
            tokio::select! {
                Some(stream) = self.ready.recv() => return Ok(stream),
                _ = self.connections.join_next(), if !self.connections.is_empty() => {},
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else { return Err(crate::Error::Closed); };
                    if self.connections.len() >= MAX_CONNECTIONS { incoming.refuse(); continue; }
                    let endpoint = self.endpoint.clone();
                    let streams = self.streams.clone();
                    self.connections.spawn(async move {
                        let Ok(Ok(connection)) = tokio::time::timeout(CONNECT_WAIT, incoming).await else { return; };
                        let Ok(fingerprint) = peer(&connection) else { connection.close(1u32.into(), b"identity"); return; };
                        while let Ok((send, recv)) = connection.accept_bi().await {
                            let stream = TransportStream::Quic(Box::new(QuicStream {
                                send, recv, connection: connection.clone(), _endpoint: endpoint.clone(),
                            }));
                            if streams.send((Framed::new(stream, Channel::Input), fingerprint,
                                connection.remote_address())).await.is_err() { break; }
                        }
                    });
                }
            }
        }
    }
}

fn peer(connection: &Connection) -> Result<Fingerprint> {
    let handshake = connection
        .handshake_data()
        .and_then(|data| data.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .ok_or(crate::Error::WrongProtocol)?;
    if handshake.protocol.as_deref() != Some(ALPN) {
        return Err(crate::Error::WrongProtocol);
    }
    let identity = connection
        .peer_identity()
        .and_then(|identity| identity.downcast::<Vec<CertificateDer<'static>>>().ok())
        .ok_or(crate::Error::NoPeerCertificate)?;
    Ok(Fingerprint::of(
        identity.first().ok_or(crate::Error::NoPeerCertificate)?,
    ))
}

fn io(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}
