//! 慢原生调用期间网络继续响应，确认前不越过输入屏障，取消后先清理再开始新一代引擎。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ferry_engine::{
    ClientConfig, EdgeFeed, Event, Events, Files, Live, ServerAddr, ServerConfig, connect, serve,
};
use ferry_platform::{CaptureEvent, CaptureSink, InputCapture, InputInject};
use ferry_platform_mock::{MockCapture, MockClipboard, MockDisplays, MockFileCommit, MockInject};
use ferry_types::{InputEvent, KeyAction, KeyCode, LockKeys, Point, Side};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::support::{AutoConfirm, Device, EDGE, collect, display, eventually};

#[derive(Clone, Default)]
struct Gate(Arc<GateState>);

#[derive(Default)]
struct GateState {
    armed: AtomicBool,
    entered: AtomicBool,
    timed_out: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
}

impl Gate {
    fn arm(&self) {
        *self.0.released.lock().unwrap() = false;
        self.0.entered.store(false, Ordering::SeqCst);
        self.0.armed.store(true, Ordering::SeqCst);
    }
    fn entered(&self) -> bool {
        self.0.entered.load(Ordering::SeqCst)
    }
    fn wait(&self) {
        if !self.0.armed.swap(false, Ordering::SeqCst) {
            return;
        }
        self.0.entered.store(true, Ordering::SeqCst);
        let (_released, expired) = self
            .0
            .wake
            .wait_timeout_while(
                self.0.released.lock().unwrap(),
                Duration::from_secs(8),
                |released| !*released,
            )
            .unwrap();
        self.0
            .timed_out
            .store(expired.timed_out(), Ordering::SeqCst);
    }
    fn release(&self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}

#[derive(Clone, Default)]
struct Capture {
    inner: MockCapture,
    grab: Gate,
    release: Gate,
    starts: Arc<AtomicUsize>,
    fail_grab: Arc<AtomicBool>,
    fail_release_once: Arc<AtomicBool>,
}

impl InputCapture for Capture {
    fn start(&mut self, sink: CaptureSink) -> ferry_platform::Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.inner.start(sink)
    }
    fn grab(&self) -> ferry_platform::Result<()> {
        self.grab.wait();
        self.inner.grab()?;
        if self.fail_grab.load(Ordering::SeqCst) {
            Err(ferry_platform::Error::Failed("模拟部分抓取后失败".into()))
        } else {
            Ok(())
        }
    }
    fn release(&self, at: Point) -> ferry_platform::Result<()> {
        self.release.wait();
        if self.fail_release_once.swap(false, Ordering::SeqCst) {
            return Err(ferry_platform::Error::Failed("模拟第一次归还失败".into()));
        }
        self.inner.release(at)
    }
    fn lock_keys(&self) -> LockKeys {
        self.inner.lock_keys()
    }
}

#[tokio::test]
async fn failed_partial_grab_with_failed_restore_terminates_and_retries_cleanup() {
    let mut rig = Rig::start().await;
    rig.capture.fail_grab.store(true, Ordering::SeqCst);
    rig.capture.fail_release_once.store(true, Ordering::SeqCst);
    rig.push(1);
    eventually("部分抓取失败使引擎结束", || {
        rig.server.is_finished()
    })
    .await;
    let error = (&mut rig.server).await.unwrap().unwrap_err();
    assert!(error.to_string().contains("平台捕获"));
    eventually("Drop再次恢复实际抓取状态", || {
        !rig.capture.inner.is_grabbed()
    })
    .await;
    assert_eq!(rig.capture.inner.last_release(), Some(EDGE));
    assert!(
        rig.inject.inner.events().is_empty(),
        "失败抓取没有发送Enter"
    );
}

#[derive(Clone, Default)]
struct Inject {
    inner: MockInject,
    hidden: Gate,
    key: Gate,
    up_failures_remaining: Arc<AtomicUsize>,
    up_failures: Arc<AtomicUsize>,
}

impl InputInject for Inject {
    fn inject(&mut self, event: &InputEvent) -> ferry_platform::Result<()> {
        if matches!(
            event,
            InputEvent::Key {
                action: KeyAction::Down,
                ..
            }
        ) {
            self.key.wait();
        }
        if matches!(
            event,
            InputEvent::Key {
                action: KeyAction::Up,
                ..
            }
        ) && self.up_failures_remaining.load(Ordering::SeqCst) > 0
        {
            self.up_failures_remaining.fetch_sub(1, Ordering::SeqCst);
            self.up_failures.fetch_add(1, Ordering::SeqCst);
            return Err(ferry_platform::Error::Failed("模拟原生Up失败".into()));
        }
        self.inner.inject(event)
    }
    fn sync_locks(&mut self, wanted: LockKeys, mask: LockKeys) -> ferry_platform::Result<()> {
        self.inner.sync_locks(wanted, mask)
    }
    fn set_cursor_hidden(&mut self, hidden: bool) -> ferry_platform::Result<()> {
        self.hidden.wait();
        self.inner.set_cursor_hidden(hidden)
    }
}

type Task = JoinHandle<ferry_engine::Result<std::convert::Infallible>>;

struct Rig {
    server: Task,
    client: Task,
    capture: Capture,
    inject: Inject,
    client_device: Device,
    server_device: Device,
    server_events: Arc<Mutex<Vec<Event>>>,
    client_events: Arc<Mutex<Vec<Event>>>,
}

fn files(device: &Device) -> Files {
    Files::new(&device.store, Events::none(), Arc::new(MockFileCommit))
}

fn server_config(device: &Device, events: Events) -> ServerConfig {
    ServerConfig {
        live: Live {
            side: Side::Right,
            layout: vec![],
            devices: vec![],
            fallback: Default::default(),
            mouse_dpi: 1600,
            input: Default::default(),
        }
        .fixed(),
        name: "capture-worker".into(),
        clipboard: false,
        clipboard_limit: Default::default(),
        command_key: true,
        store: device.store.clone(),
        announce: false,
        events,
        files: files(device),
        edge: EdgeFeed::none(),
    }
}

fn client_config(device: &Device, address: SocketAddr, events: Events) -> ClientConfig {
    ClientConfig {
        server: ServerAddr::Fixed(address),
        name: "inject-worker".into(),
        clipboard: false,
        clipboard_limit: Default::default(),
        command_key: false,
        store: device.store.clone(),
        events,
        files: files(device),
        edge: EdgeFeed::none(),
        devices: vec![],
        commands: None,
        invite_port: None,
    }
}

impl Rig {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_device = Device::new();
        let client_device = Device::new();
        let capture = Capture::default();
        let inject = Inject::default();
        let (events, server_events) = collect();
        let server = tokio::spawn(serve(
            listener,
            server_config(&server_device, events),
            capture.clone(),
            MockDisplays::new(vec![display(1000, 800, 100)]),
            MockClipboard::new(),
            AutoConfirm::answering(true),
        ));
        let (events, client_events) = collect();
        let client = tokio::spawn(connect(
            client_config(&client_device, addr, events),
            inject.clone(),
            MockDisplays::new(vec![display(1920, 1080, 150)]),
            MockClipboard::new(),
            AutoConfirm::answering(true),
        ));
        eventually("平台工作线程会话建立", || {
            !inject.inner.cursor_hidden().is_empty()
        })
        .await;
        Self {
            server,
            client,
            capture,
            inject,
            client_device,
            server_device,
            server_events,
            client_events,
        }
    }

    fn push(&self, dx: i32) {
        self.capture.inner.emit(CaptureEvent::Pointer {
            position: EDGE,
            dx,
            dy: 0,
        });
    }

    async fn enter(&self) {
        self.push(1);
        eventually("客户端显示光标", || {
            self.inject.inner.cursor_hidden().last() == Some(&false)
        })
        .await;
    }

    fn key(&self, action: KeyAction) {
        self.capture.inner.emit(CaptureEvent::Key {
            code: KeyCode::keyboard(0x04),
            action,
        });
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.server.abort();
        self.client.abort();
        self.capture.grab.release();
        self.capture.release.release();
        self.inject.hidden.release();
        self.inject.key.release();
    }
}

fn rtts(events: &Mutex<Vec<Event>>) -> usize {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, Event::Rtt { .. }))
        .count()
}

#[tokio::test]
async fn capture_confirmation_does_not_pause_heartbeats_or_publish_early_enter_leave() {
    let rig = Rig::start().await;
    rig.capture.grab.arm();
    rig.push(1);
    eventually("原生抓取被阻塞", || rig.capture.grab.entered()).await;
    let count = rtts(&rig.client_events);
    eventually("等待抓取期间客户端仍完成RTT", || {
        rtts(&rig.client_events) > count
    })
    .await;
    assert!(
        rig.inject.inner.events().is_empty(),
        "原生抓取未确认时不发送Enter"
    );
    rig.capture.grab.release();
    eventually("抓取确认后进入", || {
        rig.inject.inner.cursor_hidden().last() == Some(&false)
    })
    .await;

    rig.capture.release.arm();
    rig.push(-20);
    eventually("原生归还被阻塞", || rig.capture.release.entered()).await;
    let count = rtts(&rig.client_events);
    eventually("等待归还期间客户端仍完成RTT", || {
        rtts(&rig.client_events) > count
    })
    .await;
    assert!(rig.capture.inner.is_grabbed());
    assert_eq!(
        rig.inject.inner.cursor_hidden().last(),
        Some(&false),
        "定位归还完成前不发Leave"
    );
    rig.capture.release.release();
    eventually("确认归还后离开", || {
        rig.inject.inner.cursor_hidden().last() == Some(&true)
    })
    .await;
    assert!(!rig.capture.grab.0.timed_out.load(Ordering::SeqCst));
    assert!(!rig.capture.release.0.timed_out.load(Ordering::SeqCst));
}

#[tokio::test]
async fn client_cursor_confirmation_keeps_network_alive_and_preserves_input_order() {
    let rig = Rig::start().await;
    rig.inject.hidden.arm();
    rig.push(1);
    eventually("客户端显示光标被阻塞", || {
        rig.inject.hidden.entered()
    })
    .await;
    rig.key(KeyAction::Down);
    rig.key(KeyAction::Up);
    let before = rtts(&rig.server_events);
    eventually("客户端平台等待期间回应服务端心跳", || {
        rtts(&rig.server_events) > before + 1
    })
    .await;
    assert!(
        !rig.inject
            .inner
            .events()
            .iter()
            .any(|e| matches!(e, InputEvent::Key { .. })),
        "显示确认前不注入后续键"
    );
    rig.inject.hidden.release();
    eventually("显示确认后按序注入Up", || {
        rig.inject.inner.events().iter().any(|e| {
            matches!(
                e,
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
        .filter_map(|e| match e {
            InputEvent::Key { action, .. } => Some(action),
            _ => None,
        })
        .collect();
    assert_eq!(keys, [KeyAction::Down, KeyAction::Up]);
    assert!(!rig.inject.hidden.0.timed_out.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancelled_inflight_injection_releases_key_before_new_role_starts() {
    let rig = Rig::start().await;
    rig.enter().await;
    rig.inject.key.arm();
    rig.key(KeyAction::Down);
    eventually("按下原生调用被阻塞", || rig.inject.key.entered()).await;
    rig.client.abort();
    eventually("客户端Future已取消", || rig.client.is_finished()).await;
    let capture = Capture::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let next = tokio::spawn(serve(
        listener,
        server_config(&rig.client_device, Events::none()),
        capture.clone(),
        MockDisplays::new(vec![display(1920, 1080, 150)]),
        MockClipboard::new(),
        AutoConfirm::answering(true),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        capture.starts.load(Ordering::SeqCst),
        0,
        "新角色等旧原生调用及释放，不跨代启动"
    );
    rig.inject.key.release();
    eventually("旧按键释放后新角色可以启动", || {
        capture.starts.load(Ordering::SeqCst) == 1
    })
    .await;
    let keys: Vec<_> = rig
        .inject
        .inner
        .events()
        .into_iter()
        .filter_map(|e| match e {
            InputEvent::Key { action, .. } => Some(action),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        [KeyAction::Down, KeyAction::Up],
        "已经开始的Down即使结果无人接收也有对应Up"
    );
    assert_eq!(rig.inject.inner.cursor_hidden().last(), Some(&false));
    next.abort();
}

#[tokio::test]
async fn cancelled_grab_restores_control_before_next_capture_starts() {
    let rig = Rig::start().await;
    rig.capture.grab.arm();
    rig.push(1);
    eventually("抓取中取消引擎", || rig.capture.grab.entered()).await;
    rig.server.abort();
    eventually("控制端Future已取消", || rig.server.is_finished()).await;
    let capture = Capture::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let next = tokio::spawn(serve(
        listener,
        server_config(&rig.server_device, Events::none()),
        capture.clone(),
        MockDisplays::new(vec![display(1000, 800, 100)]),
        MockClipboard::new(),
        AutoConfirm::answering(true),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(capture.starts.load(Ordering::SeqCst), 0);
    rig.capture.grab.release();
    eventually("旧抓取恢复后新引擎开始", || {
        capture.starts.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(!rig.capture.inner.is_grabbed());
    assert_eq!(rig.capture.inner.last_release(), Some(EDGE));
    next.abort();
}

#[tokio::test]
async fn hung_capture_confirmation_ends_engine_without_waiting_for_native_return() {
    let mut rig = Rig::start().await;
    rig.capture.grab.arm();
    rig.push(1);
    eventually("原生抓取挂起", || rig.capture.grab.entered()).await;
    eventually("确认超时使引擎退出", || rig.server.is_finished()).await;
    let error = (&mut rig.server).await.unwrap().unwrap_err();
    assert!(error.to_string().contains("平台捕获"));
    assert!(
        !rig.capture.grab.0.timed_out.load(Ordering::SeqCst),
        "退出没有等待原生阻塞返回"
    );
    rig.capture.grab.release();
    eventually("迟到抓取仍归还本机", || {
        rig.capture.inner.last_release() == Some(EDGE)
    })
    .await;
    assert!(!rig.capture.inner.is_grabbed());
}

#[tokio::test]
async fn hung_injection_confirmation_closes_session_and_late_down_is_released() {
    let mut rig = Rig::start().await;
    rig.enter().await;
    rig.inject.key.arm();
    rig.key(KeyAction::Down);
    eventually("原生注入挂起", || rig.inject.key.entered()).await;
    eventually("注入确认超时结束引擎", || {
        rig.client.is_finished()
    })
    .await;
    let error = (&mut rig.client).await.unwrap().unwrap_err();
    assert!(error.to_string().contains("平台注入"));
    assert!(!rig.inject.key.0.timed_out.load(Ordering::SeqCst));
    rig.inject.key.release();
    eventually("迟到Down有清理Up", || {
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
}

#[tokio::test]
async fn session_cleanup_retries_an_up_missing_from_core_pressed_state() {
    let rig = Rig::start().await;
    rig.enter().await;
    rig.key(KeyAction::Down);
    eventually("原生收到Down", || {
        rig.inject.inner.events().iter().any(|event| {
            matches!(
                event,
                InputEvent::Key {
                    action: KeyAction::Down,
                    ..
                }
            )
        })
    })
    .await;
    rig.inject.up_failures_remaining.store(1, Ordering::SeqCst);
    rig.key(KeyAction::Up);
    eventually("第一次Up失败", || {
        rig.inject.up_failures.load(Ordering::SeqCst) == 1
    })
    .await;
    rig.server.abort();
    eventually("会话清理根据worker按键状态补发Up", || {
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
}

#[tokio::test]
async fn failed_session_reset_ends_worker_and_drop_retries_pending_release() {
    let mut rig = Rig::start().await;
    rig.enter().await;
    rig.key(KeyAction::Down);
    eventually("原生收到Down", || {
        rig.inject.inner.events().iter().any(|event| {
            matches!(
                event,
                InputEvent::Key {
                    action: KeyAction::Down,
                    ..
                }
            )
        })
    })
    .await;
    rig.inject.up_failures_remaining.store(2, Ordering::SeqCst);
    rig.key(KeyAction::Up);
    eventually("第一次Up失败", || {
        rig.inject.up_failures.load(Ordering::SeqCst) == 1
    })
    .await;
    rig.server.abort();
    eventually("reset失败不复用worker重连", || {
        rig.client.is_finished()
    })
    .await;
    let error = (&mut rig.client).await.unwrap().unwrap_err();
    assert!(error.to_string().contains("平台注入"));
    assert_eq!(rig.inject.up_failures.load(Ordering::SeqCst), 2);
    eventually("Drop保留失败释放并重试Up", || {
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
}
