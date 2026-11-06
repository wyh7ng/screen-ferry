use ferry_net::{Error, Framed};
use ferry_proto::{Channel, Codec, Message};
use tokio::io::{AsyncWriteExt, duplex};

#[tokio::test]
async fn messages_survive_a_byte_at_a_time_stream() {
    let (a, b) = duplex(1);
    let mut tx = Framed::new(a, Channel::Input);
    let mut rx = Framed::new(b, Channel::Input);

    let sent = [
        Message::Ping { nonce: 1 },
        Message::Leave { seq: 7 },
        Message::Pong { nonce: 2 },
    ];
    let send = async {
        for m in &sent {
            tx.send(m).await.unwrap();
        }
        drop(tx);
    };
    let receive = async {
        let mut got = Vec::new();
        while let Some(m) = rx.recv().await.unwrap() {
            got.push(m);
        }
        got
    };
    let ((), got) = tokio::join!(send, receive);
    assert_eq!(got, sent);
}

#[tokio::test]
async fn closing_mid_frame_is_an_error() {
    let (mut a, b) = duplex(64);
    let mut frame = Vec::new();
    Codec::new(Channel::Input)
        .encode(&Message::Ping { nonce: 9 }, &mut frame)
        .unwrap();
    a.write_all(&frame[..frame.len() - 1]).await.unwrap();
    drop(a);

    let mut rx = Framed::new(b, Channel::Input);
    assert!(matches!(rx.recv().await, Err(Error::TruncatedFrame)));
}

#[tokio::test]
async fn protocol_violations_surface_as_decode_errors() {
    let (mut a, b) = duplex(64);
    a.write_all(&[0x77, 0x77, 0, 0, 0, 0]).await.unwrap();

    let mut rx = Framed::new(b, Channel::Input);
    assert!(matches!(rx.recv().await, Err(Error::Decode(_))));
}

#[tokio::test]
async fn buffered_frames_and_a_cancelled_partial_frame_preserve_order() {
    let (mut write, read) = duplex(32768);
    let codec = Codec::new(Channel::Input);
    let mut frames = Vec::new();
    for nonce in 0..500 {
        codec.encode(&Message::Ping { nonce }, &mut frames).unwrap();
    }
    let mut last = Vec::new();
    codec
        .encode(&Message::Pong { nonce: 999 }, &mut last)
        .unwrap();
    write.write_all(&frames).await.unwrap();
    write.write_all(&last[..3]).await.unwrap();
    let mut framed = Framed::new(read, Channel::Input);
    for nonce in 0..500 {
        assert_eq!(framed.recv().await.unwrap(), Some(Message::Ping { nonce }));
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), framed.recv())
            .await
            .is_err()
    );
    write.write_all(&last[3..]).await.unwrap();
    drop(write);
    assert_eq!(
        framed.recv().await.unwrap(),
        Some(Message::Pong { nonce: 999 })
    );
    assert_eq!(framed.recv().await.unwrap(), None);
}
