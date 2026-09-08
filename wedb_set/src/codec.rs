//! 集合成员索引分块编解码器（用于 KeyTag::SetChunk = 0x08）
//! 统一复用 wrecord 的通用分块编解码实现

pub use wrecord::{ChunkCodec as MemberChunkCodec, ChunkIter as MemberChunkIter};
