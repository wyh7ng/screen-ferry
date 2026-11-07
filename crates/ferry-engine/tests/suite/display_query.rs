use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ferry_engine::{
    ClientConfig, EdgeFeed, Error, Event, Files, Live, ServerAddr, ServerConfig, connect, serve,
};
use ferry_platform::{CaptureEvent, Displays};
use ferry_platform_mock::{MockCapture, MockClipboard, MockDisplays, MockFileCommit, MockInject};
use ferry_types::{Display, InputEvent, KeyAction, KeyCode, Side};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::support::{AutoConfirm, Device, EDGE, collect, display, eventually};

#[derive(Clone)]
struct GatedDisplays(Arc<(Mutex<QueryState>, Condvar)>);

struct QueryState {
    displays: Vec<Display>,
    calls: usize,
    gate_next: bool,
    blocked: bool,
    released: bool,
    timed_out: bool,
    fail: bool,
}

impl GatedDisplays {
    fn new(displays: Vec<Display>) -> Self {
        Self(Arc::new((
            Mutex::new(QueryState {
                displays,
                calls: 0,
                gate_next: false,
                blocked: false,
                released: true,
                timed_out: false,
                fail: false,
            }),
            Condvar::new(),
        )))
    }

    fn gate(&self) {
        let mut state = self.0.0.lock().unwrap();
        state.gate_next = true;
        state.released = false;
    }

    fn release(&self) {
        self.0.0.lock().unwrap().released = true;
        self.0.1.notify_all();
    }

    fn blocked(&self) -> bool {
        self.0.0.lock().unwrap().blocked
    }

    fn calls(&self) -> usize {
        self.0.0.lock().unwrap().calls
    }

    fn set(&self, displays: Vec<Display>, fail: bool) {
        let mut state = self.0.0.lock().unwrap();
        state.displays = displays;
        state.fail = fail;
    }
}

impl Displays for GatedDisplays {
    fn displays(&self) -> ferry_platform::Result<Vec<Display>> {
        let mut state = self.0.0.lock().unwrap();
        state.calls += 1;
        let snapshot = state.displays.clone();
        let fail = state.fail;
        if std::mem::take(&mut state.gate_next) {
            state.blocked = true;
            // 回归时核心线程可能误被同步调用卡住；有限等待让失败测试也能退出。
            let (returned, timeout) = self
                .0
                .1
                .wait_timeout_while(state, Duration::from_secs(6), |s| !s.released)
                .unwrap();
            state = returned;
            state.timed_out |= timeout.timed_out();
            state.blocked = false;
        }
        if fail {
            Err(ferry_platform::Error::Failed("显示器查询暂时失败".into()))
        } else {
            Ok(snapshot)
        }
    }
}

struct QueryRig {
    addr: std::net::SocketAddr,
    server: JoinHandle<Result<std::convert::Infallible, Error>>,
    client: JoinHandle<Result<std::convert::Infallible, Error>>,
    capture: MockCapture,
    inject: MockInject,
    server_displays: GatedDisplays,
    client_displays: GatedDisplays,
    client_events: Arc<Mutex<Vec<Event>>>,
    server_events: Arc<Mutex<Vec<Event>>>,
    _server_device: Device,
    _client_device: Device,
}

impl QueryRig {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_device = Device::new();
        let client_device = Device::new();
        let (server_events, seen_server) = collect();
        let (client_events, seen_client) = collect();
        let server_displays = GatedDisplays::new(vec![display(1000, 800, 100)]);
        let client_displays = GatedDisplays::new(vec![display(1920, 1080, 150)]);
        let capture = MockCapture::new();
        let inject = MockInject::new();
        let server = tokio::spawn(serve(
            listener,
            server_config(&server_device, server_events),
            capture.clone(),
            server_displays.clone(),
            MockClipboard::new(),
            AutoConfirm::answering(true),
        ));
        let client = tokio::spawn(connect(
            ClientConfig {
                server: ServerAddr::Fixed(addr),
                name: "display-client".into(),
                clipboard: false,
                clipboard_limit: Default::default(),
                command_key: false,
                store: client_device.store.clone(),
                events: client_events,
                files: files(&client_device),
                edge: EdgeFeed::none(),
                devices: Vec::new(),
                commands: None,
                invite_port: None,
            },
            inject.clone(),
            client_displays.clone(),
            MockClipboard::new(),
            AutoConfirm::answering(true),
        ));
        eventually("显示器测试会话建立", || {
            seen_client
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::Connected { .. }))
        })
        .await;
        let rig = Self {
            addr,
            server,
            client,
            capture,
            inject,
            server_displays,
            client_displays,
            client_events: seen_client,
            server_events: seen_server,
            _server_device: server_device,
            _client_device: client_device,
        };
        eventually("进入显示器测试客户端", || {
            rig.capture.emit(CaptureEvent::Pointer {
                position: EDGE,
                dx: 1,
                dy: 0,
            });
            !rig.inject.events().is_empty()
        })
        .await;
        rig
    }

    fn key(&self) {
        self.capture.emit(CaptureEvent::Key {
            code: KeyCode::keyboard(0x04),
            action: KeyAction::Down,
        });
        self.capture.emit(CaptureEvent::Key {
            code: KeyCode::keyboard(0x04),
            action: KeyAction::Up,
        });
    }
}

impl Drop for QueryRig {
    fn drop(&mut self) {
        self.server.abort();
        self.client.abort();
        self.server_displays.release();
        self.client_displays.release();
    }
}

fn files(device: &Device) -> Files {
    Files::new(
        &device.store,
        ferry_engine::Events::none(),
        Arc::new(MockFileCommit),
    )
}

fn server_config(device: &Device, events: ferry_engine::Events) -> ServerConfig {
    ServerConfig {
        live: Live {
            side: Side::Right,
            layout: Vec::new(),
            devices: Vec::new(),
            fallback: Default::default(),
            mouse_dpi: 1600,
            input: Default::default(),
        }
        .fixed(),
        name: "display-server".into(),
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

fn rtts(events: &Mutex<Vec<Event>>) -> usize {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, Event::Rtt { .. }))
        .count()
}

async fn blocked_query_keeps_input_and_heartbeats_running(server: bool) {
    let rig = QueryRig::start().await;
    let displays = if server {
        &rig.server_displays
    } else {
        &rig.client_displays
    };
    displays.gate();
    eventually("周期查询进入阻塞平台API", || displays.blocked()).await;
    let calls = displays.calls();
    let heartbeats = rtts(&rig.client_events);
    rig.key();
    eventually("慢查询期间仍能收到按键松开", || {
        rig.inject.events().contains(&InputEvent::Key {
            code: KeyCode::keyboard(0x04),
            action: KeyAction::Up,
        })
    })
    .await;
    eventually("慢查询期间仍能完成应用心跳", || {
        rtts(&rig.client_events) > heartbeats
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(displays.blocked(), "平台调用应仍在等待，核心任务已继续工作");
    assert_eq!(displays.calls(), calls, "周期 tick 不应补排或并行查询");
    assert!(!displays.0.0.lock().unwrap().timed_out);
    displays.release();
    eventually("显示查询完成", || !displays.blocked()).await;
    let events = if server {
        &rig.server_events
    } else {
        &rig.client_events
    };
    // 完成耗时会在下个5秒窗口报告，且归属正确的本机/会话阶段。
    tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            if events.lock().unwrap().iter().any(|event| {
                matches!(event, Event::Metrics { peer, metrics }
                    if peer.is_none() == server && metrics.display_query.max_us >= 1_000_000)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("慢显示器查询应有独立耗时记录");
}

#[tokio::test]
async fn blocked_server_display_query_preserves_input_and_heartbeat_progress() {
    blocked_query_keeps_input_and_heartbeats_running(true).await;
}

#[tokio::test]
async fn blocked_client_display_query_preserves_input_and_heartbeat_progress() {
    blocked_query_keeps_input_and_heartbeats_running(false).await;
}

#[tokio::test]
async fn failed_or_empty_display_refresh_keeps_the_last_valid_layout() {
    let rig = QueryRig::start().await;
    for fail in [true, false] {
        rig.client_displays.set(Vec::new(), fail);
        let calls = rig.client_displays.calls();
        eventually("失败或空显示器刷新已执行", || {
            rig.client_displays.calls() > calls
        })
        .await;
        rig.capture.emit(CaptureEvent::Pointer {
            position: EDGE,
            dx: 2000,
            dy: 0,
        });
        eventually("仍按上一个有效范围移动", || {
            matches!(rig.inject.events().last(), Some(InputEvent::PointerMotion { position }) if position.x == 1919)
        })
        .await;
    }
    rig.client_displays
        .set(vec![display(1280, 720, 150)], false);
    eventually("恢复查询后采用新显示范围", || {
        rig.capture.emit(CaptureEvent::Pointer {
            position: EDGE,
            dx: -10,
            dy: 0,
        });
        rig.capture.emit(CaptureEvent::Pointer {
            position: EDGE,
            dx: 2000,
            dy: 0,
        });
        matches!(rig.inject.events().last(), Some(InputEvent::PointerMotion { position }) if position.x == 1279)
    })
    .await;
}

#[tokio::test]
async fn cancelling_startup_does_not_wait_for_a_blocked_native_query() {
    let device = Device::new();
    let displays = GatedDisplays::new(vec![display(1000, 800, 100)]);
    displays.gate();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let engine = tokio::spawn(serve(
        listener,
        server_config(&device, ferry_engine::Events::none()),
        MockCapture::new(),
        displays.clone(),
        MockClipboard::new(),
        AutoConfirm::answering(true),
    ));
    eventually("启动查询等待中", || displays.blocked()).await;
    let started = Instant::now();
    engine.abort();
    assert!(engine.await.unwrap_err().is_cancelled());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(displays.blocked(), "取消引擎不必等待原生调用退出");

    // 新引擎使用独立查询结果；旧线程完成不能把旧布局写到它。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (events, seen) = collect();
    let replacement = tokio::spawn(serve(
        listener,
        server_config(&device, events),
        MockCapture::new(),
        MockDisplays::new(vec![display(800, 600, 100)]),
        MockClipboard::new(),
        AutoConfirm::answering(true),
    ));
    eventually("新引擎不等旧线程即可启动", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Listening { .. }))
    })
    .await;
    displays.release();
    eventually("旧线程完成并退出", || !displays.blocked()).await;
    assert_eq!(displays.calls(), 1);
    replacement.abort();
}

#[tokio::test]
async fn an_empty_initial_display_query_is_reported_without_starting_capture() {
    let device = Device::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let capture = MockCapture::new();
    let result = serve(
        listener,
        server_config(&device, ferry_engine::Events::none()),
        capture,
        MockDisplays::new(Vec::new()),
        MockClipboard::new(),
        AutoConfirm::answering(true),
    )
    .await;
    assert!(matches!(result, Err(Error::NoDisplays("本机"))));
}

#[tokio::test]
async fn reconnect_discards_an_old_session_display_query_before_publishing_layout() {
    let mut rig = QueryRig::start().await;
    rig.client_displays.gate();
    eventually("旧会话的显示器查询在飞", || {
        rig.client_displays.blocked()
    })
    .await;
    let old_calls = rig.client_displays.calls();
    rig.server.abort();
    assert!((&mut rig.server).await.unwrap_err().is_cancelled());
    eventually("客户端确认旧会话已断开", || {
        rig.client_events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Event::Disconnected { .. }))
    })
    .await;
    let (events, seen) = collect();
    let listener = TcpListener::bind(rig.addr).await.unwrap();
    // 重连间隔1秒：旧原生查询仍阻塞时，不应先打开连接占用对端的5秒握手期限。
    assert!(
        tokio::time::timeout(Duration::from_millis(1500), listener.accept())
            .await
            .is_err(),
        "本机显示器尚未准备好，不能提前打开输入连接"
    );
    assert!(rig.client_displays.blocked());
    rig.capture = MockCapture::new();
    rig.server = tokio::spawn(serve(
        listener,
        server_config(&rig._server_device, events),
        rig.capture.clone(),
        MockDisplays::new(vec![display(1000, 800, 100)]),
        MockClipboard::new(),
        AutoConfirm::answering(true),
    ));
    rig.client_displays
        .set(vec![display(1280, 720, 150)], false);
    rig.client_displays.release();
    eventually("新会话登记完成", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Event::Connected { .. }))
    })
    .await;
    assert!(
        rig.client_displays.calls() > old_calls,
        "必须重新查询，不能使用旧结果"
    );
    let before = rig.inject.events().len();
    eventually("新会话从最新布局进入", || {
        rig.capture.emit(CaptureEvent::Pointer {
            position: EDGE,
            dx: 1,
            dy: 0,
        });
        rig.inject.events().len() > before
    })
    .await;
    rig.capture.emit(CaptureEvent::Pointer {
        position: EDGE,
        dx: 2000,
        dy: 0,
    });
    eventually("重连第一批输入已使用新范围", || {
        matches!(rig.inject.events().last(), Some(InputEvent::PointerMotion { position }) if position.x == 1279)
    })
    .await;
}
