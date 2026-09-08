//! RESP 回复值模型：模块过程与 Lua 脚本共用的统一回复表示，支持零拷贝借用写出。

use crate::error::{Error, Result};

/// RESP 回复值 (1:1 对标 Redis reply 语义)
///
/// 注意：[`Self::Status`] 与 [`Self::Error`] 载荷不得含 CRLF（RESP 以 CRLF
/// 定帧，模块为受信进程内代码，载荷合法性由模块侧保证）。
#[derive(Debug, Clone, PartialEq)]
pub enum RespValue {
  /// 简单状态回复 (+OK)
  Status(Vec<u8>),
  /// 错误回复 (-ERR msg)
  Error(Vec<u8>),
  /// 整数回复 (:n)
  Integer(i64),
  /// 批量字符串回复 ($data)，None 为 NULL
  Bulk(Option<Vec<u8>>),
  /// 数组回复 (*n)
  Array(Vec<RespValue>),
}

impl RespValue {
  /// 追加整数十进制表示（栈上缓冲，零堆分配）
  fn push_num(out: &mut Vec<u8>, n: impl itoa::Integer) {
    let mut buf = itoa::Buffer::new();
    out.extend_from_slice(buf.format(n).as_bytes());
  }

  /// 写出 RESP 编码字节流
  pub fn write_resp(&self, out: &mut Vec<u8>) {
    match self {
      Self::Status(s) => {
        out.push(b'+');
        out.extend_from_slice(s);
        out.extend_from_slice(b"\r\n");
      }
      Self::Error(e) => {
        out.push(b'-');
        out.extend_from_slice(e);
        out.extend_from_slice(b"\r\n");
      }
      Self::Integer(n) => {
        out.push(b':');
        Self::push_num(out, *n);
        out.extend_from_slice(b"\r\n");
      }
      Self::Bulk(data) => match data {
        Some(d) => {
          out.push(b'$');
          Self::push_num(out, d.len());
          out.extend_from_slice(b"\r\n");
          out.extend_from_slice(d);
          out.extend_from_slice(b"\r\n");
        }
        None => out.extend_from_slice(b"$-1\r\n"),
      },
      Self::Array(items) => {
        out.push(b'*');
        Self::push_num(out, items.len());
        out.extend_from_slice(b"\r\n");
        for item in items {
          item.write_resp(out);
        }
      }
    }
  }

  /// RESP 编码
  pub fn to_resp(&self) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    self.write_resp(&mut buf);
    buf
  }

  /// 从 Lua/模块过程常用的 (ok, msg) 构造状态回复
  pub fn ok() -> Self {
    Self::Status(b"OK".to_vec())
  }

  /// 构造错误回复
  pub fn err(msg: impl AsRef<[u8]>) -> Self {
    Self::Error(msg.as_ref().to_vec())
  }

  /// 解析执行结果为具体值；错误回复转为 Err
  pub fn into_result(self) -> Result<Self> {
    match self {
      Self::Error(e) => Err(Error::Proc(String::from_utf8_lossy(&e).into_owned())),
      v => Ok(v),
    }
  }
}
