//! 真实TLS上的接收排队计时与心跳去重；平台调用使用受控闸门。

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ferry_engine::{ClientConfig, EdgeFeed, Event, Events, Files, ServerAddr, connect};
use ferry_net::{Framed, ServerStream, Tls};
use ferry_platform::InputInject;
use ferry_platform_mock::{MockClipboard, MockDisplays, MockFileCommit, MockInject};
use ferry_proto::{Bye, ByeReason, Capabilities, Channel, Enter, Message};
use ferry_trust::{Identity, Peer, PeerList};
use ferry_types::{InputEvent, KeyAction, KeyCode, LockKeys, Point};
use tokio::task::JoinHandle;

use crate::support::{AutoConfirm, Device, collect, display, eventually};

#[derive(Default)]
struct GateState {
    blocked: bool,
    released: bool,
    timed_out: bool,
}

#[derive(Clone, Default)]
struct GatedInject {
    inner: MockInject,
    gate: Arc<(Mutex<GateState>, Condvar)>,
}

impl GatedInject {
    fn release(&self) {
        self.gate.0.lock().unwrap().released = true;
        self.gate.1.notify_all();
    }

    fn blocked(&self) -> bool {
        self.gate.0.lock().unwrap().blocked
    }
}

impl InputInject for GatedInject {
    fn inject(&mut self, event: &InputEvent) -> ferry_platform::Result<()> {
        if matches!(
            event,
            InputEvent::Key {
                action: KeyAction::Down,
                ..
            }
        ) {
            let mut state = self.gate.0.lock().unwrap();
            state.blocked = true;
            let (mut state, expired) = self
                .gate
                .1
                .wait_timeout_while(state, Duration::from_secs(3), |state| !state.released)
                .unwrap();
            state.timed_out = expired.timed_out();
            state.blocked = false;
        }
        self.inner.inject(event)
    }

    fn sync_locks(&mut self, wanted: LockKeys, mask: LockKeys) -> ferry_platform::Result<()> {
        self.inner.sync_locks(wanted, mask)
    }

    fn set_cursor_hidden(&mut self, hidden: bool) -> ferry_platform::Result<()> {
        self.inner.set_cursor_hidden(hidden)
    }
}

struct WireClient {
    wire: Framed<ServerStream>,
    inject: GatedInject,
    events: Arc<Mutex<Vec<Event>>>,
    task: JoinHandle<ferry_engine::Result<std::convert::Infallible>>,
    _device: Device,
}

impl WireClient {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Identity::generate("io-wire-server").unwrap();
        let tls = Tls::new(&server).unwrap();
        let device = Device::new();
        device
            .store
            .save_peers(&PeerList::from_iter([Peer::new(
                "io-wire-server",
                server.fingerprint(),
            )]))
            .unwrap();
        let (events, seen) = collect();
        let inject = GatedInject::default();
        let task = tokio::spawn(connect(
            ClientConfig {
                server: ServerAddr::Fixed(listener.local_addr().unwrap()),
                name: "io-wire-client".into(),
                clipboard: false,
                clipboard_limit: Default::default(),
                command_key: false,
                store: device.store.clone(),
                events,
                files: Files::new(&device.store, Events::none(), Arc::new(MockFileCommit)),
                edge: EdgeFeed::none(),
                devices: Vec::new(),
                commands: None,
                invite_port: None,
            },
            inject.clone(),
            MockDisplays::new(vec![display(1920, 1080, 100)]),
            MockClipboard::new(),
            AutoConfirm::answering(false),
        ));
        let (stream, _) = listener.accept().await.unwrap();
        let (mut wire, _) = tls.accept(stream).await.unwrap();
        let (hello, _) =
            ferry_net::accept(&mut wire, Capabilities::empty(), "io-wire-server", |_| {
                Ok(9)
            })
            .await
            .unwrap();
        assert_eq!(hello.channel, Channel::Input);
        wire.send(&Message::Displays(vec![display(1920, 1080, 100)]))
            .await
            .unwrap();
        assert!(matches!(
            wire.recv().await.unwrap(),
            Some(Message::Displays(_))
        ));
        eventually("客户端输入会话建立", || {
            seen.lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, Event::Connected { .. }))
        })
        .await;
        Self {
            wire,
            inject,
            events: seen,
            task,
            _device: device,
        }
    }

    async fn marker(&mut self, nonce: u64) -> u64 {
        self.wire.send(&Message::Ping { nonce }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            let mut answered = 0;
            loop {
                match self.wire.recv().await.unwrap() {
                    Some(Message::Pong { nonce: got }) if got == nonce => break answered,
                    Some(Message::Ping { nonce }) => {
                        self.wire.send(&Message::Pong { nonce }).await.unwrap();
                        answered += 1;
                    }
                    message => panic!("标记心跳前收到意外消息：{message:?}"),
                }
            }
        })
        .await
        .expect("平台调用等待期间reader仍回应心跳")
    }

    async fn finish(&mut self) -> Vec<ferry_engine::InputMetrics> {
        self.wire
            .send(&Message::Bye(Bye {
                reason: ByeReason::CLOSED,
            }))
            .await
            .unwrap();
        eventually("会话关闭并发布最终指标", || {
            self.events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, Event::Disconnected { .. }))
        })
        .await;
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::Metrics {
                    peer: Some(_),
                    metrics,
                } => Some((**metrics).clone()),
                _ => None,
            })
            .collect()
    }
}

impl Drop for WireClient {
    fn drop(&mut self) {
        self.task.abort();
        self.inject.release();
    }
}

#[tokio::test]
async fn injection_metrics_include_receive_queue_wait_while_reader_answers_heartbeats() {
    let mut rig = WireClient::start().await;
    rig.wire
        .send(&Message::Enter(Enter {
            seq: 1,
            position: Point::new(10, 10),
            locks: LockKeys::empty(),
        }))
        .await
        .unwrap();
    eventually("原生显示确认完成", || {
        rig.inject.inner.cursor_hidden().last() == Some(&false)
    })
    .await;
    let code = KeyCode::keyboard(0x04);
    rig.wire
        .send(&Message::Input(InputEvent::Key {
            code,
            action: KeyAction::Down,
        }))
        .await
        .unwrap();
    eventually("第一个按键原生调用被阻塞", || {
        rig.inject.blocked()
    })
    .await;
    rig.wire
        .send(&Message::Input(InputEvent::Key {
            code,
            action: KeyAction::Up,
        }))
        .await
        .unwrap();
    // 同一可靠流中的Pong证明reader已越过前面的Up，将它排进输入队列。
    rig.marker(u64::MAX - 1).await;
    assert!(rig.inject.blocked());
    assert!(
        !rig.inject
            .inner
            .events()
            .iter()
            .any(|event| matches!(event, InputEvent::Key { .. }))
    );
    // 明确注入120ms负载，不要求调度器达到某个很小的时限。
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(rig.inject.blocked());
    rig.inject.release();
    eventually("确认后按序执行到KeyUp", || {
        rig.inject.inner.events().iter().any(|event| {
            matches!(
                event,
                InputEvent::Key {
                    action: KeyAction::Up,
                    ..
                }
            )
        })
    })
    .await;
    let keys: Vec<_> = rig
        .inject
        .inner
        .events()
        .into_iter()
        .filter_map(|event| match event {
            InputEvent::Key { action, .. } => Some(action),
            _ => None,
        })
        .collect();
    assert_eq!(keys, [KeyAction::Down, KeyAction::Up]);
    assert!(!rig.inject.gate.0.lock().unwrap().timed_out);
    let windows = rig.finish().await;
    assert_eq!(
        windows
            .iter()
            .map(|metrics| metrics.inject.over_100ms)
            .sum::<u64>(),
        2,
        "慢Down及队列中等待的Up都应计入；不能只测出队后的平台调用"
    );
}

#[tokio::test]
async fn duplicate_and_unsolicited_pongs_do_not_create_rtt_samples() {
    let mut rig = WireClient::start().await;
    let nonce = tokio::time::timeout(Duration::from_secs(1), async {
        match rig.wire.recv().await.unwrap() {
            Some(Message::Ping { nonce }) => nonce,
            message => panic!("应先收到客户端测量Ping：{message:?}"),
        }
    })
    .await
    .unwrap();
    rig.wire.send(&Message::Pong { nonce }).await.unwrap();
    for _ in 0..8 {
        rig.wire.send(&Message::Pong { nonce }).await.unwrap();
    }
    rig.wire
        .send(&Message::Pong {
            nonce: nonce.saturating_sub(1),
        })
        .await
        .unwrap();
    // 后面的marker被处理后，上面的所有回复已经过reader校验。
    let additional_probes = rig.marker(u64::MAX - 2).await;
    let windows = rig.finish().await;
    assert_eq!(
        windows.iter().map(|metrics| metrics.rtt.count).sum::<u64>(),
        1 + additional_probes,
        "只记录真正尚未回复的探针，重复及未发送nonce均不产生RTT"
    );
}
