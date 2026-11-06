use ferry_proto::{Channel, Codec, Message, MotionPacket};
use ferry_types::Point;

#[test]
fn experimental_movement_has_canonical_framing_and_a_separate_datagram_magic() {
    let packet = MotionPacket {
        generation: 42,
        sequence: 9,
        barrier: 4,
        position: Point::new(-17, 234),
    };
    assert_eq!(
        MotionPacket::decode_datagram(&packet.encode_datagram()),
        Some(packet)
    );
    let mut wrong = packet.encode_datagram();
    wrong[0] = 0;
    assert_eq!(MotionPacket::decode_datagram(&wrong), None);
    assert_eq!(MotionPacket::decode_datagram(&wrong[..35]), None);
    for message in [
        Message::MotionFence {
            generation: 42,
            sequence: 8,
            position: Some(packet.position),
        },
        Message::MotionFence {
            generation: 42,
            sequence: 8,
            position: None,
        },
        Message::MotionAck {
            generation: 42,
            sequence: 8,
        },
        Message::MotionSync(packet),
    ] {
        let codec = Codec::new(Channel::Input);
        let mut bytes = Vec::new();
        codec.encode(&message, &mut bytes).unwrap();
        assert_eq!(
            codec.decode(&bytes).unwrap(),
            Some((message.clone(), bytes.len()))
        );
        assert!(
            Codec::new(Channel::Data)
                .encode(&message, &mut Vec::new())
                .is_err()
        );
    }
}
