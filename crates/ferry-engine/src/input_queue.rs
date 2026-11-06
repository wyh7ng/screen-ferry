//! 每会话有界输入队列。只替换连续的可合并绝对移动，其他消息都是严格屏障。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use ferry_proto::Message;
use ferry_types::InputEvent;
use tokio::sync::Notify;

use crate::metrics::{QueueSummary, micros};

/// 输入合并默认关闭；开启也只合并布局计算之后的无按键普通移动。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputSettings {
    pub coalesce_motion: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    Full,
    Closed,
}

/// 队列内保留的消息及其本机入队时刻；合并时使用最新消息的时刻。
pub struct QueuedInput {
    pub message: Message,
    pub queued_at: Instant,
    pub ordinary_motion: bool,
    mergeable: bool,
}

struct State {
    items: VecDeque<QueuedInput>,
    capacity: usize,
    closed: bool,
    coalesce: bool,
    high_water: usize,
    coalesced: u64,
    overflows: u64,
}

struct Shared {
    state: Mutex<State>,
    ready: Notify,
}

/// 单生产者端。销毁或显式close后，接收端排空已有消息再结束。
pub struct InputSender(Arc<Shared>);
/// 单消费者端，销毁时丢弃剩余消息并拒绝后续输入。
pub struct InputReceiver(Arc<Shared>);

/// 创建一个不等待凑批次的队列；只有发送端已经积压的相邻普通移动可合并。
pub fn input_queue(capacity: usize, settings: InputSettings) -> (InputSender, InputReceiver) {
    let capacity = capacity.max(1);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            items: VecDeque::with_capacity(capacity),
            capacity,
            closed: false,
            coalesce: settings.coalesce_motion,
            high_water: 0,
            coalesced: 0,
            overflows: 0,
        }),
        ready: Notify::new(),
    });
    (InputSender(shared.clone()), InputReceiver(shared))
}

impl InputSender {
    /// mergeable必须由布局/按键状态判断；非绝对移动即使传true也不会合并。
    pub fn try_send(&self, message: Message, mergeable: bool) -> Result<(), QueueError> {
        let now = Instant::now();
        let mut state = lock(&self.0);
        if state.closed {
            return Err(QueueError::Closed);
        }
        let ordinary_motion =
            mergeable && matches!(message, Message::Input(InputEvent::PointerMotion { .. }));
        let mergeable = state.coalesce && ordinary_motion;
        if mergeable
            && let Some(last) = state.items.back_mut()
            && last.mergeable
        {
            last.message = message;
            last.queued_at = now;
            state.coalesced = state.coalesced.saturating_add(1);
            return Ok(());
        }
        if state.items.len() >= state.capacity {
            state.overflows = state.overflows.saturating_add(1);
            return Err(QueueError::Full);
        }
        state.items.push_back(QueuedInput {
            message,
            queued_at: now,
            mergeable,
            ordinary_motion,
        });
        state.high_water = state.high_water.max(state.items.len());
        drop(state);
        self.0.ready.notify_one();
        Ok(())
    }

    /// 布局等未发送到此队列的状态变化也必须分开前后的移动。
    pub fn barrier(&self) {
        if let Some(last) = lock(&self.0).items.back_mut() {
            last.mergeable = false;
        }
    }

    pub fn configure(&self, settings: InputSettings) {
        let mut state = lock(&self.0);
        state.coalesce = settings.coalesce_motion;
        // 切换开关形成屏障；重新打开也不能回头合并旧状态的移动。
        if let Some(last) = state.items.back_mut() {
            last.mergeable = false;
        }
    }

    pub fn summary(&self) -> QueueSummary {
        summary(&self.0)
    }

    pub fn close(&self) {
        lock(&self.0).closed = true;
        self.0.ready.notify_one();
    }
}

impl Drop for InputSender {
    fn drop(&mut self) {
        self.close();
    }
}

impl InputReceiver {
    /// 可放select中；取消等待不会消费队列或丢失唤醒。
    pub async fn recv(&mut self) -> Option<QueuedInput> {
        loop {
            let ready = self.0.ready.notified();
            {
                let mut state = lock(&self.0);
                if let Some(item) = state.items.pop_front() {
                    return Some(item);
                }
                if state.closed {
                    return None;
                }
            }
            ready.await;
        }
    }
    pub fn summary(&self) -> QueueSummary {
        summary(&self.0)
    }
}

impl Drop for InputReceiver {
    fn drop(&mut self) {
        let mut state = lock(&self.0);
        state.closed = true;
        state.items.clear();
    }
}

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(|e| e.into_inner())
}

fn summary(shared: &Shared) -> QueueSummary {
    let state = lock(shared);
    QueueSummary {
        depth: state.items.len(),
        high_water: state.high_water,
        oldest_us: state
            .items
            .front()
            .map(|item| micros(item.queued_at.elapsed())),
        max_wait_us: 0,
        coalesced: state.coalesced,
        overflows: state.overflows,
    }
}
