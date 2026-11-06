use crate::support::{Rig, Setup, eventually};
use ferry_engine::{
    ClientConfig, ClipLimit, EdgeFeed, Events, Live, ServerAddr, ServerConfig, Transport,
    connect_with_transport, serve_with_transport,
};
use ferry_net::Tls;
use ferry_proto::{Capabilities, Enter, Message, MotionPacket};
use ferry_types::{DeviceSettings, InputEvent, LockKeys, Point, Side};
use std::time::Duration;

#[tokio::test]
async fn quic_pairs_shares_input_and_releases_keys_with_the_existing_engine() {
    quic_engine_case(Transport::Quic).await;
}

#[tokio::test]
async fn quic_reliable_only_baseline_keeps_pairing_input_and_tcp_data_working() {
    quic_engine_case(Transport::QuicReliable).await;
}

async fn quic_engine_case(transport: Transport) {
    let mut rig = Rig::with(Setup {
        start_client: false,
        ..Setup::default()
    })
    .await;
    rig.server.abort();
    let _ = (&mut rig.server).await;
    rig.server = start_server(&rig, transport).await;
    let config = ClientConfig {
        server: ServerAddr::Fixed(rig.addr),
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
    };
    rig.client = tokio::spawn({
        let inject = rig.inject.clone();
        let displays = rig.pc_displays.clone();
        let clipboard = rig.pc_clipboard.clone();
        let confirm = rig.pc_confirm.clone();
        async move {
            connect_with_transport(config, inject, displays, clipboard, confirm, transport)
                .await
                .unwrap_err()
        }
    });
    rig.enter().await;
    assert_eq!(rig.mac_confirm.requests().len(), 1);
    assert_eq!(rig.pc_confirm.requests().len(), 1);
    rig.push(100, 0);
    eventually("QUIC绝对移动到达", || matches!(rig.inject.events().last(), Some(InputEvent::PointerMotion { position }) if position.x > 100)).await;
    rig.key(
        ferry_types::KeyCode::LEFT_SHIFT,
        ferry_types::KeyAction::Down,
    );
    rig.leave().await;
    eventually("Leave释放按键", || {
        rig.inject.events().contains(&InputEvent::Key {
            code: ferry_types::KeyCode::LEFT_SHIFT,
            action: ferry_types::KeyAction::Up,
        })
    })
    .await;
    rig.server.abort();
    let _ = (&mut rig.server).await;
    rig.server = start_server(&rig, transport).await;
    // 不等draining：引擎内部的有界异步绑定重试负责恢复。
    rig.enter().await;
    assert_eq!(rig.mac_confirm.requests().len(), 1, "重启复用既有配对");
    assert_eq!(rig.pc_confirm.requests().len(), 1);
    rig.leave().await;
    rig.server.abort();
    rig.client.abort();
}

async fn next_ack(stream: &mut ferry_net::Framed<ferry_net::ServerStream>) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match stream.recv().await.unwrap().unwrap() {
                Message::MotionAck { .. } => return,
                Message::Ping { nonce } => stream.send(&Message::Pong { nonce }).await.unwrap(),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn datagram_loss_reordering_and_stale_sessions_are_safe_on_a_real_quic_connection() {
    let rig = Rig::with(Setup {
        start_client: false,
        ..Setup::default()
    })
    .await;
    let server = Tls::new(&rig.mac.store.identity("mac").unwrap()).unwrap();
    let mut listener = server.listen_quic("127.0.0.1:0".parse().unwrap()).unwrap();
    let config = crate::client::paired_config(&rig, listener.local_addr().unwrap());
    let client = tokio::spawn(connect_with_transport(
        config,
        rig.inject.clone(),
        rig.pc_displays.clone(),
        rig.pc_clipboard.clone(),
        rig.pc_confirm.clone(),
        Transport::Quic,
    ));
    let (mut stream, _, _) = listener.accept().await.unwrap();
    ferry_net::accept(&mut stream, Capabilities::QUIC_MOTION, "mac", |_| Ok(71))
        .await
        .unwrap();
    stream
        .send(&Message::Displays(vec![crate::support::display(
            1000, 800, 100,
        )]))
        .await
        .unwrap();
    let datagrams = stream.get_ref().datagrams().unwrap();
    let packet = MotionPacket {
        generation: 71,
        sequence: 2,
        barrier: 1,
        position: Point::new(400, 400),
    };
    datagrams.send(&packet.encode_datagram()).unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(rig.inject.events().is_empty(), "Enter之前不可注入datagram");
    stream
        .send(&Message::MotionFence {
            generation: 71,
            sequence: 1,
            position: None,
        })
        .await
        .unwrap();
    stream
        .send(&Message::Enter(Enter {
            seq: 1,
            position: Point::new(10, 10),
            locks: LockKeys::empty(),
        }))
        .await
        .unwrap();
    next_ack(&mut stream).await;
    // 模拟最后一包丢失：只发可靠补偿；其后的旧包、旧会话和重复包都不能倒退光标。
    stream.send(&Message::MotionSync(packet)).await.unwrap();
    eventually("可靠补偿恢复最后位置", || {
        rig.inject.events().last()
            == Some(&InputEvent::PointerMotion {
                position: packet.position,
            })
    })
    .await;
    let later = MotionPacket {
        sequence: 4,
        position: Point::new(700, 500),
        ..packet
    };
    datagrams.send(&later.encode_datagram()).unwrap();
    eventually("新的datagram移动", || {
        rig.inject.events().last()
            == Some(&InputEvent::PointerMotion {
                position: later.position,
            })
    })
    .await;
    let before = rig.inject.events().len();
    for stale in [
        packet,
        later,
        MotionPacket {
            generation: 70,
            sequence: 999,
            ..packet
        },
    ] {
        datagrams.send(&stale.encode_datagram()).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(rig.inject.events().len(), before);
    stream
        .send(&Message::MotionFence {
            generation: 71,
            sequence: 5,
            position: None,
        })
        .await
        .unwrap();
    stream.send(&Message::Leave { seq: 1 }).await.unwrap();
    next_ack(&mut stream).await;
    let before = rig.inject.events().len();
    datagrams
        .send(
            &MotionPacket {
                sequence: 6,
                barrier: 5,
                ..later
            }
            .encode_datagram(),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        rig.inject.events().len(),
        before,
        "Leave后迟到包不能重现光标"
    );
    client.abort();
    rig.server.abort();
}

async fn start_server(
    rig: &Rig,
    transport: Transport,
) -> tokio::task::JoinHandle<ferry_engine::Error> {
    let listener = tokio::net::TcpListener::bind(rig.addr).await.unwrap();
    let config = ServerConfig {
        live: Live {
            side: Side::Right,
            layout: Vec::new(),
            devices: Vec::new(),
            fallback: DeviceSettings::default(),
            mouse_dpi: 1600,
            input: Default::default(),
        }
        .fixed(),
        name: "mac".into(),
        clipboard: true,
        clipboard_limit: ClipLimit::default(),
        command_key: true,
        store: rig.mac.store.clone(),
        announce: false,
        events: Events::none(),
        files: rig.mac_files.clone(),
        edge: EdgeFeed::none(),
    };
    tokio::spawn({
        let capture = rig.capture.clone();
        let displays = rig.mac_displays.clone();
        let clipboard = rig.mac_clipboard.clone();
        let confirm = rig.mac_confirm.clone();
        async move {
            serve_with_transport(
                listener, config, capture, displays, clipboard, confirm, transport,
            )
            .await
            .unwrap_err()
        }
    })
}

#[tokio::test]
async fn experimental_server_rejects_data_channels_on_quic_even_for_trusted_peers() {
    use ferry_proto::{Channel, Hello, RejectReason};
    let mut rig = Rig::with(Setup {
        start_client: false,
        ..Setup::default()
    })
    .await;
    rig.server.abort();
    let _ = (&mut rig.server).await;
    let _ = crate::client::paired_config(&rig, rig.addr);
    rig.server = start_server(&rig, Transport::Quic).await;
    let tls = Tls::new(&rig.pc.store.identity("pc").unwrap()).unwrap();
    // 先通过Input建立QUIC流，再在Hello中声明错误的Data，绕过客户端transport自动pin。
    let (mut stream, _) = tls
        .connect_with(rig.addr, Channel::Input, Transport::Quic)
        .await
        .unwrap();
    let result = ferry_net::initiate(
        &mut stream,
        Hello::new(Channel::Data, Capabilities::all(), 1, "pc"),
    )
    .await;
    assert!(
        matches!(result, Err(ferry_net::Error::Rejected(reject)) if reject.reason == RejectReason::UNKNOWN_SESSION)
    );
    rig.server.abort();
}
