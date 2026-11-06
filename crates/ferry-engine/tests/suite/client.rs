use ferry_platform::CaptureEvent;
use ferry_types::{InputEvent, LockKeys, MouseButton};

use crate::support::{ENTRY, Rig, eventually};

#[tokio::test]
async fn buttons_are_released_when_the_server_disappears() {
    let rig = Rig::start().await;
    rig.enter().await;
    rig.capture.emit(CaptureEvent::Button {
        button: MouseButton::LEFT,
        pressed: true,
    });
    eventually("客户端收到按下", || rig.inject.events().len() >= 2).await;

    rig.server.abort();

    let released = InputEvent::Button {
        button: MouseButton::LEFT,
        pressed: false,
        position: ENTRY,
    };
    eventually("服务端消失后松开鼠标键", || {
        rig.inject.events().last() == Some(&released)
    })
    .await;
}

#[tokio::test]
async fn entering_syncs_caps_lock() {
    let rig = Rig::start().await;
    rig.capture.set_lock_keys(LockKeys::CAPS | LockKeys::NUM);
    rig.enter().await;

    eventually("客户端同步锁定键", || {
        !rig.inject.synced_locks().is_empty()
    })
    .await;
    assert_eq!(
        rig.inject.synced_locks(),
        [LockKeys::CAPS],
        "只同步 Caps Lock"
    );
}

#[tokio::test]
async fn keys_are_released_when_the_client_engine_is_stopped() {
    use ferry_types::{KeyAction, KeyCode};

    let rig = Rig::start().await;
    rig.enter().await;
    rig.key(KeyCode::LEFT_SHIFT, KeyAction::Down);
    let down = InputEvent::Key {
        code: KeyCode::LEFT_SHIFT,
        action: KeyAction::Down,
    };
    eventually("客户端按下 Shift", || {
        rig.inject.events().contains(&down)
    })
    .await;

    // 常驻后台暂停共享、重启引擎时，会直接丢掉客户端的任务。
    rig.client.abort();

    let up = InputEvent::Key {
        code: KeyCode::LEFT_SHIFT,
        action: KeyAction::Up,
    };
    eventually("引擎停下时松开 Shift", || {
        rig.inject.events().contains(&up)
    })
    .await;
}

#[tokio::test]
async fn the_local_cursor_is_hidden_while_the_pointer_is_on_the_server() {
    let rig = Rig::start().await;
    eventually(
        "连上后隐藏本机光标（光标在服务端）",
        || rig.inject.cursor_hidden() == [true],
    )
    .await;

    rig.enter().await;
    eventually("光标过来时显示", || {
        rig.inject.cursor_hidden() == [true, false]
    })
    .await;

    rig.leave().await;
    eventually("光标离开时再隐藏", || {
        rig.inject.cursor_hidden() == [true, false, true]
    })
    .await;

    // 服务端消失：会话结束，光标一定要恢复显示。
    rig.server.abort();
    eventually("断开后恢复显示", || {
        rig.inject.cursor_hidden() == [true, false, true, false]
    })
    .await;
}

// ── 按地址连接 ───────────────────────────────

mod by_address {
    use ferry_engine::{ClientCommand, DialError, Event};

    use crate::support::{Rig, Setup};

    async fn server_only() -> Rig {
        Rig::with(Setup {
            start_client: false,
            ..Setup::default()
        })
        .await
    }

    #[tokio::test]
    async fn a_client_connects_to_an_address_when_asked_and_pairs() {
        let rig = server_only().await;
        let mut pc = rig.idle_client("书房", false);
        pc.commands.send(ClientCommand::Connect(rig.addr)).unwrap();

        let error = pc
            .wait_for("按地址连接的结果", |e| match e {
                Event::Dialed { error, invite, .. } if !invite => Some(*error),
                _ => None,
            })
            .await;
        assert_eq!(error, None, "连上了，开始核对配对码");
        // 配对完立即用这个地址重连，不用等局域网发现。
        pc.wait_for("配对后连上服务端", |e| {
            matches!(e, Event::Connected { .. }).then_some(())
        })
        .await;
        let peers = pc.device.store.peers().unwrap();
        let server = peers.iter().next().expect("记住了服务端");
        assert_eq!(server.addr, Some(rig.addr), "记下了服务端的地址");
    }

    #[tokio::test]
    async fn an_address_where_nothing_waits_is_reported() {
        let rig = server_only().await;
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let mut pc = rig.idle_client("书房", false);
        pc.commands.send(ClientCommand::Connect(closed)).unwrap();
        let error = pc
            .wait_for("按地址连接的结果", |e| match e {
                Event::Dialed { error, .. } => Some(*error),
                _ => None,
            })
            .await;
        assert_eq!(error, Some(DialError::Refused));
    }
}

/// 一对已信任的设备，跳过配对以便精确控制输入、数据连接的顺序。
pub(crate) fn paired_config(rig: &Rig, addr: std::net::SocketAddr) -> ferry_engine::ClientConfig {
    use ferry_engine::{ClientConfig, ClipLimit, EdgeFeed, Events, ServerAddr};
    use ferry_trust::{Peer, PeerList};
    let server = rig.mac.store.identity("mac").unwrap().fingerprint();
    let client = rig.pc.store.identity("pc").unwrap().fingerprint();
    for (store, name, fingerprint) in [
        (&rig.mac.store, "pc", client),
        (&rig.pc.store, "mac", server),
    ] {
        let mut peers = PeerList::new();
        peers.add(Peer {
            name: name.into(),
            fingerprint,
            addr: None,
        });
        store.save_peers(&peers).unwrap();
    }
    ClientConfig {
        server: ServerAddr::Fixed(addr),
        name: "pc".into(),
        clipboard: true,
        clipboard_limit: ClipLimit::default(),
        command_key: false,
        store: rig.pc.store.clone(),
        events: Events::none(),
        files: rig.pc_files.clone(),
        edge: EdgeFeed::none(),
        devices: Vec::new(),
        commands: None,
        invite_port: None,
    }
}

#[tokio::test]
async fn a_slow_data_connection_and_its_reconnect_do_not_interrupt_input() {
    use crate::support::Setup;
    use ferry_engine::connect;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tokio::{
        net::{TcpListener, TcpStream},
        sync::Notify,
        task::JoinSet,
    };

    let rig = Rig::with(Setup {
        start_client: false,
        ..Setup::default()
    })
    .await;
    rig.mac_clipboard
        .copy(ferry_types::ClipboardContent::text("before data reconnect"));
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = proxy.local_addr().unwrap();
    let config = paired_config(&rig, address);
    let connections = Arc::new(AtomicUsize::new(0));
    let close_data = Arc::new(Notify::new());
    let relay = tokio::spawn({
        let connections = connections.clone();
        let close_data = close_data.clone();
        let to = rig.addr;
        async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = proxy.accept() => {
                        let (mut from, _) = accepted.unwrap();
                        let number = connections.fetch_add(1, Ordering::SeqCst);
                        let close_data = close_data.clone();
                        tasks.spawn(async move {
                            // 第二条连接是数据；延迟长于服务端1.5秒的心跳超时。
                            if number == 1 { tokio::time::sleep(Duration::from_millis(1800)).await; }
                            let mut target = TcpStream::connect(to).await.unwrap();
                            tokio::select! {
                                _ = tokio::io::copy_bidirectional(&mut from, &mut target) => {},
                                _ = close_data.notified(), if number == 1 => {},
                            }
                        });
                    }
                    _ = tasks.join_next(), if !tasks.is_empty() => {},
                }
            }
        }
    });
    let client = tokio::spawn(connect(
        config,
        rig.inject.clone(),
        rig.pc_displays.clone(),
        rig.pc_clipboard.clone(),
        rig.pc_confirm.clone(),
    ));
    rig.enter().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(rig.capture.is_grabbed(), "数据握手期间输入仍连着");
    eventually("数据连接完成", || {
        !rig.pc_files.connected().is_empty()
    })
    .await;
    assert_eq!(connections.load(Ordering::SeqCst), 2);
    eventually("初次剪贴板同步", || rig.pc_clipboard.writes() == 1).await;
    rig.pc_clipboard.copy(ferry_types::ClipboardContent::text(
        "latest on active client",
    ));
    close_data.notify_one();
    eventually("数据连接单独重建", || {
        connections.load(Ordering::SeqCst) >= 3
    })
    .await;
    eventually("文件能力恢复", || {
        !rig.pc_files.connected().is_empty()
    })
    .await;
    assert!(rig.capture.is_grabbed(), "数据断开不归还/重建输入会话");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rig.pc_clipboard.content().as_text(),
        Some("latest on active client"),
        "数据重连不能用服务端旧剪贴板覆盖活动端"
    );
    rig.leave().await;
    eventually("返回后拉取客户端的新内容", || {
        rig.mac_clipboard.content().as_text() == Some("latest on active client")
    })
    .await;
    rig.enter().await;
    assert_eq!(connections.load(Ordering::SeqCst), 3);
    client.abort();
    relay.abort();
    rig.server.abort();
}

#[tokio::test]
async fn requested_connect_reports_the_original_request_id() {
    use crate::support::Setup;
    use ferry_engine::{ClientCommand, DialError, Event};
    let rig = Rig::with(Setup {
        start_client: false,
        ..Setup::default()
    })
    .await;
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let mut pc = rig.idle_client("请求编号", false);
    pc.commands
        .send(ClientCommand::RequestedConnect {
            addr: closed,
            request_id: 42,
        })
        .unwrap();
    let reported = pc
        .wait_for("带请求编号的连接结果", |event| match event {
            Event::Dialed {
                request_id, error, ..
            } => Some((*request_id, *error)),
            _ => None,
        })
        .await;
    assert_eq!(reported, (Some(42), Some(DialError::Refused)));
}

#[tokio::test]
async fn the_clients_disabled_file_setting_removes_file_channels_in_both_directions() {
    use crate::support::Setup;
    use ferry_engine::connect;
    use ferry_types::DeviceSettings;
    let rig = Rig::with(Setup {
        start_client: false,
        ..Setup::default()
    })
    .await;
    let mut config = paired_config(&rig, rig.addr);
    let mut device = DeviceSettings::new(
        rig.mac
            .store
            .identity("mac")
            .unwrap()
            .fingerprint()
            .to_hex(),
    );
    device.files = false;
    config.devices.push(device);
    let client = tokio::spawn(connect(
        config,
        rig.inject.clone(),
        rig.pc_displays.clone(),
        rig.pc_clipboard.clone(),
        rig.pc_confirm.clone(),
    ));
    rig.mac_clipboard
        .copy(ferry_types::ClipboardContent::text("clipboard still works"));
    rig.enter().await;
    eventually("剪贴板通道仍然工作", || {
        rig.pc_clipboard.writes() > 0
    })
    .await;
    assert!(rig.pc_files.connected().is_empty());
    assert!(rig.mac_files.connected().is_empty());
    client.abort();
    rig.server.abort();
}
