//! 输入会话独立收发。只有writer写可靠流；reader可在平台调用等待期间回应心跳。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ferry_net::Framed;
use ferry_proto::Message;
use ferry_trust::Fingerprint;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::info;

use crate::heartbeat::Heartbeat;
use crate::metrics::MetricsWindow;
use crate::{Event, Events, Result};

const WRITES: usize = 128;
const READS: usize = 256;
const WRITE_DEADLINE: Duration = Duration::from_millis(500);

struct Write {
    message: Message,
    queued: Instant,
}

struct Received {
    message: Message,
    decoded_at: Instant,
}

struct Shared {
    heartbeat: Heartbeat,
    last_heard: Instant,
    metrics: MetricsWindow,
}

/// 一个可靠流的有界唯一写入队列；入队不等待网络，失败结束整个会话。
#[derive(Clone)]
pub(crate) struct Sender {
    queue: mpsc::Sender<Write>,
}

impl Sender {
    pub fn send(&self, message: Message) -> Result<()> {
        self.queue
            .try_send(Write {
                message,
                queued: Instant::now(),
            })
            .map_err(|_| closed("输入写入队列已满或停止"))
    }

    /// 为同一输入的Fence/控制消息及心跳留出空间，背压时停止取应用outbox。
    pub fn available(&self) -> bool {
        self.queue.capacity() >= 4
    }

    pub async fn writable(&self) -> Result<()> {
        let permits = self
            .queue
            .reserve_many(4)
            .await
            .map_err(|_| closed("输入写入已停止"))?;
        drop(permits);
        Ok(())
    }
}

pub(crate) struct SessionIo {
    incoming: mpsc::Receiver<Received>,
    received_at: Instant,
    sender: Sender,
    failure: watch::Receiver<Option<String>>,
    shared: Arc<Mutex<Shared>>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl SessionIo {
    pub fn new<S>(framed: Framed<S>, peer: Fingerprint, events: Events) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut read, mut write) = framed.split();
        let (queue, mut writes) = mpsc::channel::<Write>(WRITES);
        let sender = Sender { queue };
        let (messages, incoming) = mpsc::channel(READS);
        let (failed, failure) = watch::channel(None);
        let shared = Arc::new(Mutex::new(Shared {
            heartbeat: Heartbeat::new(),
            last_heard: Instant::now(),
            metrics: MetricsWindow::default(),
        }));
        let writer = tokio::spawn({
            let failed = failed.clone();
            let shared = shared.clone();
            async move {
                while let Some(queued) = writes.recv().await {
                    let heartbeat =
                        matches!(queued.message, Message::Ping { .. } | Message::Pong { .. });
                    let deadline = tokio::time::Instant::from_std(queued.queued + WRITE_DEADLINE);
                    let result =
                        tokio::time::timeout_at(deadline, write.send(&queued.message)).await;
                    {
                        let mut state = lock(&shared);
                        if heartbeat {
                            state.metrics.record_heartbeat_send(queued.queued.elapsed());
                        } else {
                            state.metrics.send.record(queued.queued.elapsed());
                        }
                    }
                    let error = match result {
                        Ok(Ok(())) => continue,
                        Ok(Err(error)) => error.to_string(),
                        Err(_) => "输入发送超时（含排队）".to_owned(),
                    };
                    failed.send_replace(Some(error));
                    break;
                }
            }
        });
        let reader = tokio::spawn({
            let sender = sender.clone();
            let shared = shared.clone();
            async move {
                loop {
                    let message = match read.recv().await {
                        Ok(Some(message)) => message,
                        Ok(None) => break,
                        Err(error) => {
                            failed.send_replace(Some(error.to_string()));
                            break;
                        }
                    };
                    lock(&shared).last_heard = Instant::now();
                    match message {
                        Message::Ping { nonce } => {
                            if let Err(error) = sender.send(Message::Pong { nonce }) {
                                failed.send_replace(Some(error.to_string()));
                                break;
                            }
                        }
                        Message::Pong { nonce } => {
                            let rtt = {
                                let mut state = lock(&shared);
                                let rtt = state.heartbeat.pong(nonce);
                                if let Some(rtt) = rtt {
                                    state.metrics.rtt.record(rtt);
                                }
                                rtt
                            };
                            if let Some(rtt) = rtt {
                                events.emit(Event::Rtt { peer, rtt });
                            }
                        }
                        message => {
                            // 接收方处理停滞时不无限回放旧输入；终止连接后按原清理规则松开按键。
                            if messages
                                .try_send(Received {
                                    message,
                                    decoded_at: Instant::now(),
                                })
                                .is_err()
                            {
                                failed.send_replace(Some("输入接收队列已满或停止".into()));
                                break;
                            }
                        }
                    }
                }
            }
        });
        Self {
            incoming,
            received_at: Instant::now(),
            sender,
            failure,
            shared,
            reader,
            writer,
        }
    }

    pub fn sender(&self) -> Sender {
        self.sender.clone()
    }

    pub async fn recv(&mut self) -> Result<Option<Message>> {
        if let Some(error) = self.failure.borrow().clone() {
            return Err(closed(&error));
        }
        tokio::select! {
            biased;
            changed = self.failure.changed() => {
                if changed.is_ok() && let Some(error) = self.failure.borrow().clone() {
                    return Err(closed(&error));
                }
                Ok(None)
            }
            message = self.incoming.recv() => Ok(message.map(|received| {
                self.received_at = received.decoded_at;
                received.message
            })),
        }
    }

    pub fn received_at(&self) -> Instant {
        self.received_at
    }

    pub fn ping(&self) -> Result<()> {
        let nonce = lock(&self.shared).heartbeat.ping();
        self.sender.send(Message::Ping { nonce })
    }

    pub fn poll_ping(&self, active: bool) -> Result<()> {
        let nonce = lock(&self.shared).heartbeat.poll_ping(active);
        if let Some(nonce) = nonce {
            self.sender.send(Message::Ping { nonce })?;
        }
        Ok(())
    }

    pub fn timed_out(&self) -> bool {
        lock(&self.shared).heartbeat.timed_out()
    }
    pub fn silence(&self) -> Duration {
        lock(&self.shared).last_heard.elapsed()
    }

    pub fn report(&self) {
        if let Some(report) = lock(&self.shared).heartbeat.poll_report() {
            info!("{report}");
        }
    }

    pub fn collect(&self, metrics: &mut MetricsWindow) {
        let mut state = lock(&self.shared);
        metrics.send.merge(&mut state.metrics.send);
        metrics
            .heartbeat_send
            .merge(&mut state.metrics.heartbeat_send);
        metrics.rtt.merge(&mut state.metrics.rtt);
    }
}

impl Drop for SessionIo {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|error| error.into_inner())
}

fn closed(message: &str) -> crate::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, message).into()
}
