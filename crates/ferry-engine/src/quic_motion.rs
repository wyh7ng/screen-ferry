//! QUIC位置实验的顺序状态机。时间仅用本机Instant，控制始终可靠；无ACK时退回可靠发送。
use ferry_proto::{Message, MotionPacket};
use ferry_types::{InputEvent, Point};
use std::time::{Duration, Instant};

const COMPENSATE: Duration = Duration::from_millis(20);
const ACK_WAIT: Duration = Duration::from_secs(2);

pub struct MotionPlan {
    pub reliable: Vec<Message>,
    pub datagram: Option<MotionPacket>,
}

pub struct MotionSender {
    generation: u64,
    sequence: u64,
    barrier: u64,
    acked: u64,
    active: bool,
    disabled: bool,
    fence_at: Option<Instant>,
    pending: Option<MotionPacket>,
    due: Option<Instant>,
}

impl MotionSender {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            sequence: 0,
            barrier: 0,
            acked: 0,
            active: false,
            disabled: false,
            fence_at: None,
            pending: None,
            due: None,
        }
    }

    pub fn prepare(&mut self, message: Message, ordinary: bool, now: Instant) -> MotionPlan {
        self.check_timeout(now);
        self.sequence = self.sequence.checked_add(1).expect("会话输入序号耗尽");
        if ordinary
            && self.active
            && let Message::Input(InputEvent::PointerMotion { position }) = message
        {
            let packet = MotionPacket {
                generation: self.generation,
                sequence: self.sequence,
                barrier: self.barrier,
                position,
            };
            if ordinary && !self.disabled && self.acked == self.barrier {
                self.pending = Some(packet);
                self.due.get_or_insert(now + COMPENSATE);
                return MotionPlan {
                    reliable: Vec::new(),
                    datagram: Some(packet),
                };
            }
            // 等屏障ACK期间，普通移动仍可靠发送，但不生成新控制屏障。
            // 否则持续移动会让ACK永远追不上不断变化的barrier。
            self.pending = None;
            self.due = None;
            return MotionPlan {
                reliable: vec![Message::MotionSync(packet)],
                datagram: None,
            };
        }
        let position = self.pending.take().map(|p| p.position);
        self.due = None;
        self.barrier = self.sequence;
        self.fence_at.get_or_insert(now);
        match &message {
            Message::Enter(_) => self.active = true,
            Message::Leave { .. } => self.active = false,
            _ => {}
        }
        MotionPlan {
            datagram: None,
            reliable: vec![
                Message::MotionFence {
                    generation: self.generation,
                    sequence: self.barrier,
                    position,
                },
                message,
            ],
        }
    }

    pub fn acknowledge(&mut self, generation: u64, sequence: u64) {
        if generation == self.generation && sequence > self.acked && sequence <= self.barrier {
            self.acked = sequence;
            // 旧屏障有序推进也说明对方正在响应；只有当前屏障确认后才能恢复datagram。
            self.fence_at = (sequence != self.barrier).then(Instant::now);
        }
    }
    pub fn check_timeout(&mut self, now: Instant) {
        if self
            .fence_at
            .is_some_and(|at| now.saturating_duration_since(at) >= ACK_WAIT)
        {
            self.disabled = true;
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.due
    }
    pub fn compensate(&mut self) -> Option<Message> {
        self.due = None;
        self.pending.take().map(Message::MotionSync)
    }
    pub fn disable(&mut self) {
        self.disabled = true;
    }
}

pub struct MotionReceiver {
    generation: u64,
    sequence: u64,
    barrier: u64,
    pending: Option<u64>,
    active: bool,
}

impl MotionReceiver {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            sequence: 0,
            barrier: 0,
            pending: None,
            active: false,
        }
    }
    pub fn fence(
        &mut self,
        generation: u64,
        sequence: u64,
        position: Option<Point>,
    ) -> Result<Option<Point>, &'static str> {
        if generation != self.generation
            || sequence <= self.barrier
            || sequence < self.sequence
            || self.pending.is_some()
        {
            return Err("QUIC移动屏障顺序不符");
        }
        self.barrier = sequence;
        self.sequence = sequence;
        self.pending = Some(sequence);
        Ok(position.filter(|_| self.active))
    }
    /// 在紧随Fence的可靠控制消息已注入/处理之后才能回ACK。
    pub fn after_control(&mut self, enters: bool, leaves: bool) -> Option<Message> {
        if enters {
            self.active = true;
        }
        if leaves {
            self.active = false;
        }
        self.pending.take().map(|sequence| Message::MotionAck {
            generation: self.generation,
            sequence,
        })
    }
    pub fn position(&mut self, packet: MotionPacket) -> Option<Point> {
        if !self.active
            || self.pending.is_some()
            || packet.generation != self.generation
            || packet.barrier != self.barrier
            || packet.sequence <= self.sequence
        {
            return None;
        }
        self.sequence = packet.sequence;
        Some(packet.position)
    }
}
