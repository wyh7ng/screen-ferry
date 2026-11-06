use ferry_net::{Tls, Transport};
use ferry_proto::{Capabilities, Channel, Hello, Message};
use ferry_trust::Identity;
use std::time::Duration;

#[tokio::test]
async fn quic_authenticates_both_devices_and_reuses_framing_on_reliable_streams() {
    let server_id = Identity::generate("server").unwrap();
    let client_id = Identity::generate("client").unwrap();
    let server = Tls::new(&server_id).unwrap();
    let client = Tls::new(&client_id).unwrap();
    let mut listener = server.listen_quic("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = client_id.fingerprint();
    let task = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, fingerprint, _) = listener.accept().await.unwrap();
            assert_eq!(fingerprint, peer);
            assert!(stream.get_ref().is_quic());
            ferry_net::accept(&mut stream, Capabilities::empty(), "server", |_| Ok(1))
                .await
                .unwrap();
            assert_eq!(
                stream.recv().await.unwrap(),
                Some(Message::Ping { nonce: 42 })
            );
            stream.send(&Message::Pong { nonce: 42 }).await.unwrap();
        }
        // 保持监听器直到最后回复被读取，类似真正server主循环。
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    for channel in [Channel::Pair, Channel::Input] {
        let (mut stream, fingerprint) = client
            .connect_with(addr, channel, Transport::Quic)
            .await
            .unwrap();
        assert_eq!(fingerprint, server_id.fingerprint());
        ferry_net::initiate(
            &mut stream,
            Hello::new(channel, Capabilities::empty(), 0, "client"),
        )
        .await
        .unwrap();
        stream.send(&Message::Ping { nonce: 42 }).await.unwrap();
        assert_eq!(
            stream.recv().await.unwrap(),
            Some(Message::Pong { nonce: 42 })
        );
    }
    task.await.unwrap();
}

#[tokio::test]
async fn automatic_transport_falls_back_only_before_a_tcp_session_is_started() {
    let server = Tls::new(&Identity::generate("server").unwrap()).unwrap();
    let client = Tls::new(&Identity::generate("client").unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let (mut stream, _) = server.accept(tcp).await.unwrap();
        ferry_net::accept(&mut stream, Capabilities::empty(), "server", |_| Ok(1))
            .await
            .unwrap();
    });
    let (mut stream, _) = client
        .connect_with(addr, Channel::Input, Transport::Auto)
        .await
        .unwrap();
    assert!(!stream.get_ref().is_quic());
    assert_eq!(stream.get_ref().fallback_reason(), Some("quic_unreachable"));
    ferry_net::initiate(
        &mut stream,
        Hello::new(Channel::Input, Capabilities::empty(), 0, "client"),
    )
    .await
    .unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn dropping_the_listener_closes_quic_and_a_new_handshake_checks_the_new_identity() {
    let old_id = Identity::generate("old").unwrap();
    let client = Tls::new(&Identity::generate("client").unwrap()).unwrap();
    let old = Tls::new(&old_id).unwrap();
    let mut listener = old.listen_quic("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _, _) = listener.accept().await.unwrap();
        stream.recv().await.unwrap();
        stream.send(&Message::Pong { nonce: 1 }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
    let (mut stream, fingerprint) = client
        .connect_with(addr, Channel::Input, Transport::Quic)
        .await
        .unwrap();
    assert_eq!(fingerprint, old_id.fingerprint());
    stream.send(&Message::Ping { nonce: 1 }).await.unwrap();
    assert_eq!(
        stream.recv().await.unwrap(),
        Some(Message::Pong { nonce: 1 })
    );
    task.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .unwrap()
            .is_err()
    );
    drop(stream);
    let new_id = Identity::generate("new").unwrap();
    let new = Tls::new(&new_id).unwrap();
    // QUIC close会保留短暂draining时间，让已发的关闭帧完成；原端口应有界释放。
    let mut listener = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match new.listen_quic(addr) {
                Ok(listener) => break listener,
                Err(ferry_net::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::AddrInUse =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => panic!("unexpected bind error: {error}"),
            }
        }
    })
    .await
    .expect("QUIC停止后UDP端口应有界释放");
    let task = tokio::spawn(async move {
        let (mut stream, _, _) = listener.accept().await.unwrap();
        stream.recv().await.unwrap();
        stream.send(&Message::Pong { nonce: 2 }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
    let (mut stream, fingerprint) = client
        .connect_with(addr, Channel::Input, Transport::Quic)
        .await
        .unwrap();
    assert_eq!(fingerprint, new_id.fingerprint());
    assert_ne!(fingerprint, old_id.fingerprint());
    stream.send(&Message::Ping { nonce: 2 }).await.unwrap();
    assert_eq!(
        stream.recv().await.unwrap(),
        Some(Message::Pong { nonce: 2 })
    );
    task.await.unwrap();
}

#[tokio::test]
async fn multi_address_attempt_budget_preserves_tcp_fallback_when_udp_is_blackholed() {
    let server = Tls::new(&Identity::generate("server").unwrap()).unwrap();
    let client = Tls::new(&Identity::generate("client").unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // UDP套接字保持开放但不回应，确保不是快速ICMP拒绝。
    let _blackhole = tokio::net::UdpSocket::bind(addr).await.unwrap();
    let task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let (mut stream, _) = server.accept(tcp).await.unwrap();
        ferry_net::accept(&mut stream, Capabilities::empty(), "server", |_| Ok(1))
            .await
            .unwrap();
    });
    let (mut stream, _) = tokio::time::timeout(
        Transport::Auto.connect_budget(Duration::from_secs(2)),
        client.connect_with(addr, Channel::Input, Transport::Auto),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!stream.get_ref().is_quic());
    ferry_net::initiate(
        &mut stream,
        Hello::new(Channel::Input, Capabilities::empty(), 0, "client"),
    )
    .await
    .unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn file_and_clipboard_channels_stay_on_tcp_even_when_quic_is_requested() {
    let server = Tls::new(&Identity::generate("server").unwrap()).unwrap();
    let client = Tls::new(&Identity::generate("client").unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        for _ in 0..2 {
            let (tcp, _) = listener.accept().await.unwrap();
            let (mut stream, _) = server.accept(tcp).await.unwrap();
            assert!(!stream.get_ref().is_quic());
            ferry_net::accept(&mut stream, Capabilities::all(), "server", |_| Ok(1))
                .await
                .unwrap();
        }
    });
    for channel in [Channel::Data, Channel::File] {
        let (mut stream, _) = tokio::time::timeout(
            Duration::from_secs(1),
            client.connect_with(addr, channel, Transport::Quic),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!stream.get_ref().is_quic());
        ferry_net::initiate(
            &mut stream,
            Hello::new(channel, Capabilities::all(), 1, "client"),
        )
        .await
        .unwrap();
    }
    task.await.unwrap();
}

#[tokio::test]
async fn a_slow_peer_does_not_hold_the_global_connection_pool_lock() {
    let server = Tls::new(&Identity::generate("server").unwrap()).unwrap();
    let client = Tls::new(&Identity::generate("client").unwrap()).unwrap();
    let blackhole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let slow = tokio::spawn({
        let client = client.clone();
        let addr = blackhole.local_addr().unwrap();
        async move {
            client
                .connect_with(addr, Channel::Input, Transport::Quic)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut listener = server.listen_quic("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _, _) = listener.accept().await.unwrap();
        ferry_net::accept(&mut stream, Capabilities::empty(), "server", |_| Ok(1))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let (mut stream, _) = tokio::time::timeout(
        Duration::from_secs(1),
        client.connect_with(addr, Channel::Input, Transport::Quic),
    )
    .await
    .unwrap()
    .unwrap();
    ferry_net::initiate(
        &mut stream,
        Hello::new(Channel::Input, Capabilities::empty(), 0, "client"),
    )
    .await
    .unwrap();
    task.await.unwrap();
    slow.abort();
}
