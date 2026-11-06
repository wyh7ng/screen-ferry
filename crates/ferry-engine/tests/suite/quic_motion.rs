use ferry_engine::{MotionReceiver, MotionSender};
use ferry_proto::{Enter, Message, MotionPacket};
use ferry_types::{InputEvent, KeyAction, KeyCode, LockKeys, Point};
use std::time::{Duration, Instant};

fn motion(x: i32) -> Message {
    Message::Input(InputEvent::PointerMotion {
        position: Point::new(x, 50),
    })
}
fn enter() -> Message {
    Message::Enter(Enter {
        seq: 1,
        position: Point::new(10, 50),
        locks: LockKeys::empty(),
    })
}

fn acknowledge(sender: &mut MotionSender, receiver: &mut MotionReceiver, plan: Vec<Message>) {
    assert_eq!(plan.len(), 2);
    let Message::MotionFence {
        generation,
        sequence,
        position,
    } = plan[0]
    else {
        panic!("missing fence");
    };
    receiver.fence(generation, sequence, position).unwrap();
    let ack = receiver
        .after_control(
            matches!(plan[1], Message::Enter(_)),
            matches!(plan[1], Message::Leave { .. }),
        )
        .unwrap();
    let Message::MotionAck {
        generation,
        sequence,
    } = ack
    else {
        unreachable!()
    };
    sender.acknowledge(generation, sequence);
}

#[test]
fn loss_reordering_duplicates_and_old_generations_cannot_move_backwards() {
    let now = Instant::now();
    let (mut sender, mut receiver) = (MotionSender::new(7), MotionReceiver::new(7));
    let initial = sender.prepare(enter(), false, now);
    acknowledge(&mut sender, &mut receiver, initial.reliable);
    let first = sender.prepare(motion(100), true, now).datagram.unwrap();
    let last = sender.prepare(motion(200), true, now).datagram.unwrap();
    assert_eq!(receiver.position(last), Some(Point::new(200, 50)));
    assert_eq!(receiver.position(first), None);
    assert_eq!(receiver.position(last), None);
    assert_eq!(
        receiver.position(MotionPacket {
            generation: 8,
            sequence: 999,
            ..last
        }),
        None
    );
    let final_lost = sender.prepare(motion(300), true, now).datagram.unwrap();
    // 最后一个datagram丢失：20ms补偿deadline不随新的移动无限延后。
    assert_eq!(sender.deadline(), Some(now + Duration::from_millis(20)));
    let Message::MotionSync(compensation) = sender.compensate().unwrap() else {
        unreachable!()
    };
    assert_eq!(compensation, final_lost);
    assert_eq!(receiver.position(compensation), Some(Point::new(300, 50)));
    assert!(sender.deadline().is_none());
}

#[test]
fn key_barriers_flush_final_position_and_wait_for_ack_without_delaying_controls() {
    let now = Instant::now();
    let (mut sender, mut receiver) = (MotionSender::new(7), MotionReceiver::new(7));
    let initial = sender.prepare(enter(), false, now);
    acknowledge(&mut sender, &mut receiver, initial.reliable);
    let old = sender.prepare(motion(100), true, now).datagram.unwrap();
    let key = Message::Input(InputEvent::Key {
        code: KeyCode::LEFT_SHIFT,
        action: KeyAction::Down,
    });
    let plan = sender.prepare(key.clone(), false, now);
    assert_eq!(plan.reliable[1], key);
    let Message::MotionFence {
        generation,
        sequence,
        position,
    } = plan.reliable[0]
    else {
        unreachable!()
    };
    assert_eq!(position, Some(old.position));
    assert_eq!(
        receiver.fence(generation, sequence, position).unwrap(),
        position
    );
    assert_eq!(receiver.position(old), None);
    // ACK只能在key已注入后发出，等待期间新的普通移动照常可靠发送，不能越过key。
    let waiting = sender.prepare(motion(120), true, now);
    assert!(waiting.datagram.is_none());
    let Message::MotionSync(packet) = waiting.reliable[0] else {
        panic!("waiting move must be reliable");
    };
    let Message::MotionAck {
        generation,
        sequence,
    } = receiver.after_control(false, false).unwrap()
    else {
        unreachable!()
    };
    assert_eq!(receiver.position(packet), Some(Point::new(120, 50)));
    sender.acknowledge(generation, sequence);
    assert!(sender.prepare(motion(130), true, now).datagram.is_some());
}

#[test]
fn lost_ack_disables_datagrams_in_bounded_time_and_leaving_rejects_late_positions() {
    let now = Instant::now();
    let (mut sender, mut receiver) = (MotionSender::new(7), MotionReceiver::new(7));
    let initial = sender.prepare(enter(), false, now);
    let Message::MotionFence {
        generation,
        sequence,
        position,
    } = initial.reliable[0]
    else {
        unreachable!()
    };
    receiver.fence(generation, sequence, position).unwrap();
    receiver.after_control(true, false); // ACK丢失
    sender.check_timeout(now + Duration::from_secs(3));
    sender.acknowledge(7, sequence);
    assert!(
        sender
            .prepare(motion(100), true, now + Duration::from_secs(3))
            .datagram
            .is_none()
    );
    let leave = sender.prepare(
        Message::Leave { seq: 1 },
        false,
        now + Duration::from_secs(3),
    );
    acknowledge(&mut sender, &mut receiver, leave.reliable);
    assert_eq!(
        receiver.position(MotionPacket {
            generation: 7,
            sequence: 100,
            barrier: 3,
            position: Point::new(999, 999)
        }),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn the_final_packet_is_compensated_on_the_fixed_timer_even_without_more_input() {
    let now = tokio::time::Instant::now();
    let mut sender = MotionSender::new(7);
    let mut receiver = MotionReceiver::new(7);
    let initial = sender.prepare(enter(), false, now.into_std());
    acknowledge(&mut sender, &mut receiver, initial.reliable);
    sender.prepare(motion(100), true, now.into_std());
    tokio::time::advance(Duration::from_millis(10)).await;
    sender.prepare(motion(200), true, tokio::time::Instant::now().into_std());
    let deadline = sender.deadline().unwrap();
    assert_eq!(deadline, (now + Duration::from_millis(20)).into_std());
    tokio::time::sleep_until(deadline.into()).await;
    let Message::MotionSync(packet) = sender.compensate().unwrap() else {
        unreachable!()
    };
    assert_eq!(receiver.position(packet), Some(Point::new(200, 50)));
}

#[test]
fn delayed_control_ack_can_catch_up_while_movement_remains_continuous() {
    let now = Instant::now();
    let (mut sender, mut receiver) = (MotionSender::new(7), MotionReceiver::new(7));
    let initial = sender.prepare(enter(), false, now);
    let Message::MotionFence {
        generation,
        sequence,
        position,
    } = initial.reliable[0]
    else {
        unreachable!()
    };
    receiver.fence(generation, sequence, position).unwrap();
    let ack = receiver.after_control(true, false).unwrap();
    for i in 0..100 {
        let plan = sender.prepare(motion(100 + i), true, now + Duration::from_millis(i as u64));
        assert!(plan.datagram.is_none());
        let Message::MotionSync(packet) = plan.reliable[0] else {
            panic!("movement must not advance control barrier");
        };
        assert!(receiver.position(packet).is_some());
    }
    let Message::MotionAck {
        generation,
        sequence,
    } = ack
    else {
        unreachable!()
    };
    sender.acknowledge(generation, sequence);
    assert!(
        sender
            .prepare(motion(201), true, now + Duration::from_millis(101))
            .datagram
            .is_some()
    );
}

#[test]
fn a_reliable_edge_move_is_a_barrier_that_later_datagrams_cannot_skip() {
    let now = Instant::now();
    let (mut sender, mut receiver) = (MotionSender::new(7), MotionReceiver::new(7));
    let initial = sender.prepare(enter(), false, now);
    acknowledge(&mut sender, &mut receiver, initial.reliable);
    let edge = sender.prepare(motion(1), false, now);
    assert!(matches!(edge.reliable[0], Message::MotionFence { .. }));
    let later = sender.prepare(motion(100), true, now);
    assert!(
        later.datagram.is_none(),
        "内部位置不能用datagram越过尚未确认的边缘路径"
    );
    acknowledge(&mut sender, &mut receiver, edge.reliable);
    let Message::MotionSync(packet) = later.reliable[0] else {
        unreachable!()
    };
    assert_eq!(receiver.position(packet), Some(Point::new(100, 50)));
    assert!(sender.prepare(motion(120), true, now).datagram.is_some());
}
