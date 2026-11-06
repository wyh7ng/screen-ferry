//! QUIC实验移动包。独立魔数、固定长度、会话代次和可靠屏障；普通TCP不发送这些消息。
use ferry_types::Point;

/// 只表示绝对位置，不能携带按键、滚轮或拖拽状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MotionPacket {
    pub generation: u64,
    pub sequence: u64,
    pub barrier: u64,
    pub position: Point,
}

impl MotionPacket {
    pub const LEN: usize = 36;
    pub fn encode_datagram(self) -> [u8; Self::LEN] {
        let mut bytes = [0; Self::LEN];
        bytes[..4].copy_from_slice(b"SFQ1");
        bytes[4..12].copy_from_slice(&self.generation.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[20..28].copy_from_slice(&self.barrier.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.position.x.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.position.y.to_le_bytes());
        bytes
    }
    pub fn decode_datagram(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::LEN || bytes[..4] != *b"SFQ1" {
            return None;
        }
        Some(Self {
            generation: u64::from_le_bytes(bytes[4..12].try_into().ok()?),
            sequence: u64::from_le_bytes(bytes[12..20].try_into().ok()?),
            barrier: u64::from_le_bytes(bytes[20..28].try_into().ok()?),
            position: Point::new(
                i32::from_le_bytes(bytes[28..32].try_into().ok()?),
                i32::from_le_bytes(bytes[32..36].try_into().ok()?),
            ),
        })
    }
}
