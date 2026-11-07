//! 帧读写：在任意字节流上收发协议消息。

use ferry_proto::{Channel, Codec, Message};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

use crate::error::{Error, Result};

/// 每次从流中至少预留这么多字节再读。
const READ_CHUNK: usize = 4 * 1024;

/// 一条连接上的消息收发。读写缓冲区复用，稳定运行时不再分配内存。
pub struct Framed<S> {
    io: S,
    codec: Codec,
    read_buf: Vec<u8>,
    read_pos: usize,
    write_buf: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Framed<S> {
    pub fn new(io: S, channel: Channel) -> Self {
        Self {
            io,
            codec: Codec::new(channel),
            read_buf: Vec::with_capacity(READ_CHUNK),
            read_pos: 0,
            write_buf: Vec::with_capacity(READ_CHUNK),
        }
    }

    pub fn get_mut(&mut self) -> &mut S {
        &mut self.io
    }

    pub fn get_ref(&self) -> &S {
        &self.io
    }

    /// 本连接的类型，决定允许哪些消息、单帧多大。
    pub fn channel(&self) -> Channel {
        self.codec.channel()
    }

    /// 拆开读取和唯一写入方；握手时预读的完整/半帧仍归读取方所有。
    pub fn split(self) -> (FramedReader<ReadHalf<S>>, FramedWriter<WriteHalf<S>>) {
        let (read, write) = tokio::io::split(self.io);
        (
            FramedReader {
                io: read,
                codec: self.codec,
                read_buf: self.read_buf,
                read_pos: self.read_pos,
            },
            FramedWriter {
                io: write,
                codec: self.codec,
                write_buf: self.write_buf,
            },
        )
    }

    /// 接受方收到 `Hello` 后才知道连接类型，由握手流程切换过去。
    pub(crate) fn set_channel(&mut self, channel: Channel) {
        self.codec = Codec::new(channel);
    }

    /// 发送一条消息。
    pub async fn send(&mut self, message: &Message) -> Result<()> {
        self.write_buf.clear();
        self.codec.encode(message, &mut self.write_buf)?;
        self.io.write_all(&self.write_buf).await?;
        self.io.flush().await?;
        Ok(())
    }

    /// 接收一条消息。对方在帧边界上正常关闭连接时返回 `None`。
    ///
    /// 可以放在 `select!` 中：被取消时已读到的数据保留在缓冲区里，不会丢失。
    pub async fn recv(&mut self) -> Result<Option<Message>> {
        loop {
            if let Some((message, used)) = self.codec.decode(&self.read_buf[self.read_pos..])? {
                self.read_pos += used;
                return Ok(Some(message));
            }
            if self.read_pos == self.read_buf.len() {
                self.read_buf.clear();
                self.read_pos = 0;
            } else if self.read_pos >= READ_CHUNK && self.read_pos >= self.read_buf.len() / 2 {
                self.read_buf.drain(..self.read_pos);
                self.read_pos = 0;
            }
            self.read_buf.reserve(READ_CHUNK);
            if self.io.read_buf(&mut self.read_buf).await? == 0 {
                return if self.read_pos == self.read_buf.len() {
                    Ok(None)
                } else {
                    Err(Error::TruncatedFrame)
                };
            }
        }
    }
}

/// 独立有序读取方，取消recv保留已经预读的数据。
pub struct FramedReader<R> {
    io: R,
    codec: Codec,
    read_buf: Vec<u8>,
    read_pos: usize,
}

impl<R: AsyncRead + Unpin> FramedReader<R> {
    pub async fn recv(&mut self) -> Result<Option<Message>> {
        loop {
            if let Some((message, used)) = self.codec.decode(&self.read_buf[self.read_pos..])? {
                self.read_pos += used;
                return Ok(Some(message));
            }
            if self.read_pos == self.read_buf.len() {
                self.read_buf.clear();
                self.read_pos = 0;
            } else if self.read_pos >= READ_CHUNK && self.read_pos >= self.read_buf.len() / 2 {
                self.read_buf.drain(..self.read_pos);
                self.read_pos = 0;
            }
            self.read_buf.reserve(READ_CHUNK);
            if self.io.read_buf(&mut self.read_buf).await? == 0 {
                return if self.read_pos == self.read_buf.len() {
                    Ok(None)
                } else {
                    Err(Error::TruncatedFrame)
                };
            }
        }
    }
}

/// 独立写入方，一条连接只能由该方依次编码、写入和flush。
pub struct FramedWriter<W> {
    io: W,
    codec: Codec,
    write_buf: Vec<u8>,
}

impl<W: AsyncWrite + Unpin> FramedWriter<W> {
    pub async fn send(&mut self, message: &Message) -> Result<()> {
        self.write_buf.clear();
        self.codec.encode(message, &mut self.write_buf)?;
        self.io.write_all(&self.write_buf).await?;
        self.io.flush().await?;
        Ok(())
    }
}
