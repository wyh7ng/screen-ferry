use std::time::Duration;

use ferry_engine::{InputSettings, QueueError, input_queue};
use ferry_proto::{Enter, Message};
use ferry_types::{InputEvent, KeyAction, KeyCode, LockKeys, MouseButton, Point};

fn motion(x: i32) -> Message {
    Message::Input(InputEvent::PointerMotion {
        position: Point::new(x, 100),
    })
}

fn enabled() -> InputSettings {
    InputSettings {
        coalesce_motion: true,
    }
}

#[tokio::test]
async fn only_adjacent_eligible_moves_are_replaced_without_waiting_for_a_batch() {
    let (tx, mut rx) = input_queue(3, enabled());
    tx.try_send(motion(1), true).unwrap();
    tx.try_send(motion(2), true).unwrap();
    assert_eq!(tx.summary().depth, 1);
    assert_eq!(tx.summary().coalesced, 1);
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(10), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .message,
        motion(2)
    );
    assert_eq!(rx.summary().depth, 0);
    assert_eq!(rx.summary().oldest_us, None);
}

#[tokio::test]
async fn all_critical_messages_and_ineligible_moves_are_ordering_barriers() {
    let barriers = [
        Message::Input(InputEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            action: KeyAction::Down,
        }),
        Message::Input(InputEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            action: KeyAction::Up,
        }),
        Message::Input(InputEvent::Button {
            button: MouseButton::LEFT,
            pressed: true,
            position: Point::new(3, 100),
        }),
        Message::Input(InputEvent::Wheel {
            dx: 0,
            dy: 120,
            position: Point::new(3, 100),
        }),
        Message::Enter(Enter {
            seq: 1,
            position: Point::new(3, 100),
            locks: LockKeys::empty(),
        }),
        Message::Leave { seq: 1 },
        Message::Input(InputEvent::PointerDelta { dx: 2, dy: 1 }),
    ];
    for barrier in barriers {
        let (tx, mut rx) = input_queue(4, enabled());
        tx.try_send(motion(1), true).unwrap();
        tx.try_send(motion(2), true).unwrap();
        tx.try_send(barrier.clone(), true).unwrap();
        tx.try_send(motion(4), true).unwrap();
        tx.try_send(motion(5), true).unwrap();
        tx.close();
        let mut received = Vec::new();
        while let Some(item) = rx.recv().await {
            received.push(item.message);
        }
        assert_eq!(received, [motion(2), barrier, motion(5)]);
    }
    let (tx, mut rx) = input_queue(4, enabled());
    for x in 0..4 {
        tx.try_send(motion(x), false).unwrap();
    }
    tx.close();
    for x in 0..4 {
        assert_eq!(rx.recv().await.unwrap().message, motion(x));
    }
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn defaults_and_reconfiguration_preserve_already_queued_boundaries() {
    let (tx, mut rx) = input_queue(8, InputSettings::default());
    tx.try_send(motion(1), true).unwrap();
    tx.try_send(motion(2), true).unwrap();
    tx.configure(enabled());
    tx.try_send(motion(3), true).unwrap();
    tx.try_send(motion(4), true).unwrap();
    tx.configure(InputSettings::default());
    tx.try_send(motion(5), true).unwrap();
    tx.close();
    let mut received = Vec::new();
    while let Some(item) = rx.recv().await {
        received.push(item.message);
    }
    assert_eq!(received, [motion(1), motion(2), motion(4), motion(5)]);
}

#[tokio::test]
async fn a_full_queue_reports_failure_without_discarding_a_key_release() {
    let (tx, mut rx) = input_queue(2, enabled());
    let down = Message::Input(InputEvent::Key {
        code: KeyCode::LEFT_SHIFT,
        action: KeyAction::Down,
    });
    let up = Message::Input(InputEvent::Key {
        code: KeyCode::LEFT_SHIFT,
        action: KeyAction::Up,
    });
    tx.try_send(down.clone(), false).unwrap();
    tx.try_send(motion(2), false).unwrap();
    assert_eq!(tx.try_send(up.clone(), false), Err(QueueError::Full));
    assert_eq!(tx.summary().depth, 2);
    assert_eq!(tx.summary().high_water, 2);
    assert_eq!(tx.summary().overflows, 1);
    assert!(tx.summary().oldest_us.is_some());
    assert_eq!(rx.recv().await.unwrap().message, down);
    tx.try_send(up.clone(), false).unwrap();
    assert_eq!(rx.recv().await.unwrap().message, motion(2));
    assert_eq!(rx.recv().await.unwrap().message, up);
    drop(rx);
    assert_eq!(tx.try_send(motion(3), false), Err(QueueError::Closed));
}

#[tokio::test]
async fn dropping_the_sender_wakes_a_waiting_receiver() {
    let (tx, mut rx) = input_queue(2, enabled());
    let receiver = tokio::spawn(async move { rx.recv().await.is_none() });
    tokio::task::yield_now().await;
    drop(tx);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), receiver)
            .await
            .unwrap()
            .unwrap()
    );
}

#[tokio::test]
async fn cancelling_a_receive_does_not_consume_the_next_message() {
    let (tx, mut rx) = input_queue(2, enabled());
    assert!(
        tokio::time::timeout(Duration::from_millis(1), rx.recv())
            .await
            .is_err()
    );
    tx.try_send(motion(8), true).unwrap();
    assert_eq!(rx.recv().await.unwrap().message, motion(8));
}

#[tokio::test]
async fn layout_barriers_keep_moves_from_different_coordinate_spaces_separate() {
    let (tx, mut rx) = input_queue(3, enabled());
    tx.try_send(motion(1), true).unwrap();
    tx.barrier();
    tx.try_send(motion(2), true).unwrap();
    tx.try_send(motion(3), true).unwrap();
    assert_eq!(rx.recv().await.unwrap().message, motion(1));
    assert_eq!(rx.recv().await.unwrap().message, motion(3));
}
