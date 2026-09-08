use bytes::Bytes;
use wedb_resp::{RespWriteUtils, Result as RespResult};

use crate::error::{Error, Result};

/// 发布订阅消息类型枚举
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubSubMessage {
  /// 普通频道广播消息: message <channel> <payload>
  Message { channel: Bytes, payload: Bytes },
  /// 模式匹配广播消息: pmessage <pattern> <channel> <payload>
  PMessage {
    pattern: Bytes,
    channel: Bytes,
    payload: Bytes,
  },
  /// 订阅成功通知: subscribe <channel> <count>
  Subscribe { channel: Bytes, count: usize },
  /// 退订成功通知: unsubscribe <channel> <count>
  Unsubscribe {
    channel: Option<Bytes>,
    count: usize,
  },
  /// 模式订阅成功通知: psubscribe <pattern> <count>
  PSubscribe { pattern: Bytes, count: usize },
  /// 模式退订成功通知: punsubscribe <pattern> <count>
  PUnsubscribe {
    pattern: Option<Bytes>,
    count: usize,
  },
  /// 内部断开连接停机信号
  Close,
}

/// 帧前缀常量对 `[RESP2 数组头, RESP3 推送帧头]`（`*<n>\r\n` / `><n>\r\n` + 命令名 Bulk 串），
/// 配合 `resp3 as usize` 索引选取；两种 RESP 的同命令前缀长度恒等
const MSG_FRAME: [&[u8]; 2] = [b"*3\r\n$7\r\nmessage\r\n", b">3\r\n$7\r\nmessage\r\n"];
const PMSG_FRAME: [&[u8]; 2] = [b"*4\r\n$8\r\npmessage\r\n", b">4\r\n$8\r\npmessage\r\n"];
const SUB_FRAME: [&[u8]; 2] = [b"*3\r\n$9\r\nsubscribe\r\n", b">3\r\n$9\r\nsubscribe\r\n"];
const UNSUB_FRAME: [&[u8]; 2] = [
  b"*3\r\n$11\r\nunsubscribe\r\n",
  b">3\r\n$11\r\nunsubscribe\r\n",
];
const PSUB_FRAME: [&[u8]; 2] = [
  b"*3\r\n$10\r\npsubscribe\r\n",
  b">3\r\n$10\r\npsubscribe\r\n",
];
const PUNSUB_FRAME: [&[u8]; 2] = [
  b"*3\r\n$12\r\npunsubscribe\r\n",
  b">3\r\n$12\r\npunsubscribe\r\n",
];

/// 空值字节长度常量对 `[RESP2 空块 $-1\r\n, RESP3 空值 _\r\n]`
const NULL_LEN: [usize; 2] = [5, 3];

/// 选取帧前缀
#[inline]
const fn frame(pair: [&[u8]; 2], resp3: bool) -> &[u8] {
  pair[resp3 as usize]
}

/// 计算非负整数的十进制字符长度，纯位运算加速，零堆开销
#[inline]
const fn digit_len(val: usize) -> usize {
  if val == 0 {
    1
  } else {
    val.ilog10() as usize + 1
  }
}

/// 计算 Bulk String 所需的 RESP 字节长度: $<len>\r\n<payload>\r\n
#[inline]
const fn bulk_len(payload_len: usize) -> usize {
  5 + digit_len(payload_len) + payload_len
}

/// 计算整数所需的 RESP 字节长度: :<val>\r\n
#[inline]
const fn int_len(val: usize) -> usize {
  3 + digit_len(val)
}

/// 追加 Bulk String 到缓冲区
#[inline]
fn append_bulk_string(out: &mut Vec<u8>, data: &[u8]) {
  out.push(b'$');
  let mut buf = itoa::Buffer::new();
  out.extend_from_slice(buf.format(data.len()).as_bytes());
  out.extend_from_slice(b"\r\n");
  out.extend_from_slice(data);
  out.extend_from_slice(b"\r\n");
}

/// 追加整数到缓冲区
#[inline]
fn append_int(out: &mut Vec<u8>, val: usize) {
  out.push(b':');
  let mut buf = itoa::Buffer::new();
  out.extend_from_slice(buf.format(val).as_bytes());
  out.extend_from_slice(b"\r\n");
}

/// 追加空值（无参退订）：RESP2 空块 `$-1\r\n` / RESP3 空值 `_\r\n`
#[inline]
fn append_null(out: &mut Vec<u8>, resp3: bool) {
  out.extend_from_slice(if resp3 { b"_\r\n" } else { b"$-1\r\n" });
}

/// 写入空值（无参退订）到固定切片
#[inline]
fn write_null(out: &mut [u8], resp3: bool) -> RespResult<usize> {
  if resp3 {
    RespWriteUtils::write_resp3_null(out)
  } else {
    RespWriteUtils::write_null(out)
  }
}

impl PubSubMessage {
  /// 创建普通消息
  #[inline]
  pub fn message(channel: impl Into<Bytes>, payload: impl Into<Bytes>) -> Self {
    Self::Message {
      channel: channel.into(),
      payload: payload.into(),
    }
  }

  /// 创建模式匹配消息
  #[inline]
  pub fn pmessage(
    pattern: impl Into<Bytes>,
    channel: impl Into<Bytes>,
    payload: impl Into<Bytes>,
  ) -> Self {
    Self::PMessage {
      pattern: pattern.into(),
      channel: channel.into(),
      payload: payload.into(),
    }
  }

  /// 创建订阅确认通知
  #[inline]
  pub fn subscribe(channel: impl Into<Bytes>, count: usize) -> Self {
    Self::Subscribe {
      channel: channel.into(),
      count,
    }
  }

  /// 创建退订确认通知
  #[inline]
  pub fn unsubscribe(channel: impl Into<Bytes>, count: usize) -> Self {
    Self::Unsubscribe {
      channel: Some(channel.into()),
      count,
    }
  }

  /// 创建全退订空通知
  #[inline]
  pub fn unsubscribe_null(count: usize) -> Self {
    Self::Unsubscribe {
      channel: None,
      count,
    }
  }

  /// 创建模式订阅确认通知
  #[inline]
  pub fn psubscribe(pattern: impl Into<Bytes>, count: usize) -> Self {
    Self::PSubscribe {
      pattern: pattern.into(),
      count,
    }
  }

  /// 创建模式退订确认通知
  #[inline]
  pub fn punsubscribe(pattern: impl Into<Bytes>, count: usize) -> Self {
    Self::PUnsubscribe {
      pattern: Some(pattern.into()),
      count,
    }
  }

  /// 创建模式全退订空通知
  #[inline]
  pub fn punsubscribe_null(count: usize) -> Self {
    Self::PUnsubscribe {
      pattern: None,
      count,
    }
  }

  /// 计算序列化精确所需字节长度
  pub fn serialized_len(&self, resp3: bool) -> usize {
    let null_len = NULL_LEN[resp3 as usize];
    match self {
      Self::Message { channel, payload } => {
        MSG_FRAME[0].len() + bulk_len(channel.len()) + bulk_len(payload.len())
      }
      Self::PMessage {
        pattern,
        channel,
        payload,
      } => {
        PMSG_FRAME[0].len()
          + bulk_len(pattern.len())
          + bulk_len(channel.len())
          + bulk_len(payload.len())
      }
      Self::Subscribe { channel, count } => {
        SUB_FRAME[0].len() + bulk_len(channel.len()) + int_len(*count)
      }
      Self::Unsubscribe { channel, count } => {
        let ch_len = channel.as_ref().map_or(null_len, |ch| bulk_len(ch.len()));
        UNSUB_FRAME[0].len() + ch_len + int_len(*count)
      }
      Self::PSubscribe { pattern, count } => {
        PSUB_FRAME[0].len() + bulk_len(pattern.len()) + int_len(*count)
      }
      Self::PUnsubscribe { pattern, count } => {
        let pat_len = pattern.as_ref().map_or(null_len, |pat| bulk_len(pat.len()));
        PUNSUB_FRAME[0].len() + pat_len + int_len(*count)
      }
      Self::Close => 0,
    }
  }

  /// 精准预分配并编码写入给定的 Vec 缓冲区，零内存二次清零开销
  pub fn encode_to_vec(&self, out: &mut Vec<u8>, resp3: bool) {
    out.reserve(self.serialized_len(resp3));
    match self {
      Self::Message { channel, payload } => {
        out.extend_from_slice(frame(MSG_FRAME, resp3));
        append_bulk_string(out, channel);
        append_bulk_string(out, payload);
      }
      Self::PMessage {
        pattern,
        channel,
        payload,
      } => {
        out.extend_from_slice(frame(PMSG_FRAME, resp3));
        append_bulk_string(out, pattern);
        append_bulk_string(out, channel);
        append_bulk_string(out, payload);
      }
      Self::Subscribe { channel, count } => {
        out.extend_from_slice(frame(SUB_FRAME, resp3));
        append_bulk_string(out, channel);
        append_int(out, *count);
      }
      Self::Unsubscribe { channel, count } => {
        out.extend_from_slice(frame(UNSUB_FRAME, resp3));
        match channel {
          Some(ch) => append_bulk_string(out, ch),
          None => append_null(out, resp3),
        }
        append_int(out, *count);
      }
      Self::PSubscribe { pattern, count } => {
        out.extend_from_slice(frame(PSUB_FRAME, resp3));
        append_bulk_string(out, pattern);
        append_int(out, *count);
      }
      Self::PUnsubscribe { pattern, count } => {
        out.extend_from_slice(frame(PUNSUB_FRAME, resp3));
        match pattern {
          Some(pat) => append_bulk_string(out, pat),
          None => append_null(out, resp3),
        }
        append_int(out, *count);
      }
      Self::Close => {}
    }
  }

  /// 序列化写入目标字节切片
  pub fn write_to(&self, out: &mut [u8], resp3: bool) -> RespResult<usize> {
    let mut offset = 0;
    macro_rules! bulk {
      ($data:expr) => {{
        offset += RespWriteUtils::write_bulk_string(&mut out[offset..], $data)?;
      }};
    }
    macro_rules! int {
      ($val:expr) => {{
        offset += RespWriteUtils::write_int64(&mut out[offset..], $val as i64)?;
      }};
    }
    match self {
      Self::Message { channel, payload } => {
        offset += RespWriteUtils::write_direct(&mut out[offset..], frame(MSG_FRAME, resp3))?;
        bulk!(channel);
        bulk!(payload);
      }
      Self::PMessage {
        pattern,
        channel,
        payload,
      } => {
        offset += RespWriteUtils::write_direct(&mut out[offset..], frame(PMSG_FRAME, resp3))?;
        bulk!(pattern);
        bulk!(channel);
        bulk!(payload);
      }
      Self::Subscribe { channel, count } => {
        offset += RespWriteUtils::write_direct(&mut out[offset..], frame(SUB_FRAME, resp3))?;
        bulk!(channel);
        int!(*count);
      }
      Self::Unsubscribe { channel, count } => {
        offset += RespWriteUtils::write_direct(&mut out[offset..], frame(UNSUB_FRAME, resp3))?;
        match channel {
          Some(ch) => bulk!(ch),
          None => offset += write_null(&mut out[offset..], resp3)?,
        }
        int!(*count);
      }
      Self::PSubscribe { pattern, count } => {
        offset += RespWriteUtils::write_direct(&mut out[offset..], frame(PSUB_FRAME, resp3))?;
        bulk!(pattern);
        int!(*count);
      }
      Self::PUnsubscribe { pattern, count } => {
        offset += RespWriteUtils::write_direct(&mut out[offset..], frame(PUNSUB_FRAME, resp3))?;
        match pattern {
          Some(pat) => bulk!(pat),
          None => offset += write_null(&mut out[offset..], resp3)?,
        }
        int!(*count);
      }
      Self::Close => {}
    }
    Ok(offset)
  }

  /// 序列化为 Bytes（RESP2 格式）
  #[inline]
  pub fn to_resp2(&self) -> Bytes {
    self.to_resp_bytes(false)
  }

  /// 序列化为 Bytes（RESP3 Push 推送帧格式）
  #[inline]
  pub fn to_resp3(&self) -> Bytes {
    self.to_resp_bytes(true)
  }

  /// 序列化为 Bytes（RESP2 数组格式，别名兼容）
  #[inline]
  pub fn to_resp2_bytes(&self) -> Bytes {
    self.to_resp2()
  }

  /// 序列化为 Bytes（RESP3 Push 推送帧格式，别名兼容）
  #[inline]
  pub fn to_resp3_bytes(&self) -> Bytes {
    self.to_resp3()
  }

  /// 通用序列化为 Bytes（利用精准预分配，杜绝二次内存清零）
  pub fn to_resp_bytes(&self, resp3: bool) -> Bytes {
    let mut buf = Vec::new();
    self.encode_to_vec(&mut buf, resp3);
    Bytes::from(buf)
  }

  /// 序列化为 Bitcode 二进制
  pub fn to_bitcode(&self) -> Vec<u8> {
    let wire = match self {
      Self::Message { channel, payload } => WireMessage::Message {
        channel: channel.to_vec(),
        payload: payload.to_vec(),
      },
      Self::PMessage {
        pattern,
        channel,
        payload,
      } => WireMessage::PMessage {
        pattern: pattern.to_vec(),
        channel: channel.to_vec(),
        payload: payload.to_vec(),
      },
      Self::Subscribe { channel, count } => WireMessage::Subscribe {
        channel: channel.to_vec(),
        count: *count,
      },
      Self::Unsubscribe { channel, count } => WireMessage::Unsubscribe {
        channel: channel.as_ref().map(|b| b.to_vec()),
        count: *count,
      },
      Self::PSubscribe { pattern, count } => WireMessage::PSubscribe {
        pattern: pattern.to_vec(),
        count: *count,
      },
      Self::PUnsubscribe { pattern, count } => WireMessage::PUnsubscribe {
        pattern: pattern.as_ref().map(|b| b.to_vec()),
        count: *count,
      },
      Self::Close => WireMessage::Close,
    };
    bitcode::encode(&wire)
  }

  /// 从 Bitcode 二进制还原发布订阅消息
  pub fn from_bitcode(bytes: &[u8]) -> Result<Self> {
    let wire: WireMessage = bitcode::decode(bytes).map_err(|e| Error::Bitcode(e.to_string()))?;
    Ok(match wire {
      WireMessage::Message { channel, payload } => Self::Message {
        channel: Bytes::from(channel),
        payload: Bytes::from(payload),
      },
      WireMessage::PMessage {
        pattern,
        channel,
        payload,
      } => Self::PMessage {
        pattern: Bytes::from(pattern),
        channel: Bytes::from(channel),
        payload: Bytes::from(payload),
      },
      WireMessage::Subscribe { channel, count } => Self::Subscribe {
        channel: Bytes::from(channel),
        count,
      },
      WireMessage::Unsubscribe { channel, count } => Self::Unsubscribe {
        channel: channel.map(Bytes::from),
        count,
      },
      WireMessage::PSubscribe { pattern, count } => Self::PSubscribe {
        pattern: Bytes::from(pattern),
        count,
      },
      WireMessage::PUnsubscribe { pattern, count } => Self::PUnsubscribe {
        pattern: pattern.map(Bytes::from),
        count,
      },
      WireMessage::Close => Self::Close,
    })
  }
}

/// Bitcode 内部编解码格式
#[derive(bitcode::Encode, bitcode::Decode)]
enum WireMessage {
  Message {
    channel: Vec<u8>,
    payload: Vec<u8>,
  },
  PMessage {
    pattern: Vec<u8>,
    channel: Vec<u8>,
    payload: Vec<u8>,
  },
  Subscribe {
    channel: Vec<u8>,
    count: usize,
  },
  Unsubscribe {
    channel: Option<Vec<u8>>,
    count: usize,
  },
  PSubscribe {
    pattern: Vec<u8>,
    count: usize,
  },
  PUnsubscribe {
    pattern: Option<Vec<u8>>,
    count: usize,
  },
  Close,
}
