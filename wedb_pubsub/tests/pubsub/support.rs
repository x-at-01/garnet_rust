use std::{
  fmt::Write,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

use parking_lot::Mutex;
use wedb_pubsub::{
  AsyncRx, PubSubMessage, SessionHandle, SubscribeBroker, create_session, glob_match,
};
use whasher::{GxPapayaMap, GxPapayaSet, new_papaya_map, new_papaya_set};

/// 模拟轻量级客户端会话（对标 Garnet 测试中的客户端连接状态机）
pub struct MockSession {
  pub broker: Arc<SubscribeBroker>,
  pub session: SessionHandle,
  pub rx: Mutex<AsyncRx<PubSubMessage>>,
  pub is_resp3: AtomicBool,
  pub subscribed_channels: GxPapayaSet<Vec<u8>>,
  pub subscribed_patterns: GxPapayaSet<Vec<u8>>,
  pub kv_store: GxPapayaMap<String, String>,
}

impl MockSession {
  pub fn new(broker: Arc<SubscribeBroker>, id: u64) -> Self {
    let (session, rx) = create_session(id, 1024);
    Self {
      broker,
      session,
      rx: Mutex::new(rx),
      is_resp3: AtomicBool::new(false),
      subscribed_channels: new_papaya_set(),
      subscribed_patterns: new_papaya_set(),
      kv_store: new_papaya_map(),
    }
  }

  pub fn subscription_count(&self) -> usize {
    self.subscribed_channels.pin().len() + self.subscribed_patterns.pin().len()
  }

  pub fn is_in_sub_mode(&self) -> bool {
    self.subscription_count() > 0
  }

  pub fn try_recv(&self) -> Result<PubSubMessage, crossfire::TryRecvError> {
    self.rx.lock().try_recv()
  }

  pub fn execute(&self, cmd: &str) -> String {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    self.execute_tokens(&tokens)
  }

  pub fn execute_tokens(&self, tokens: &[&str]) -> String {
    if tokens.is_empty() {
      return "-ERR empty command\r\n".to_string();
    }
    let upper = tokens[0].to_ascii_uppercase();

    // RESP2 订阅限制模式白名单校验
    if !self.is_resp3.load(Ordering::Acquire) && self.is_in_sub_mode() {
      let allowed = matches!(
        upper.as_str(),
        "SUBSCRIBE" | "PSUBSCRIBE" | "UNSUBSCRIBE" | "PUNSUBSCRIBE" | "PING" | "QUIT"
      );
      if !allowed {
        return format!(
          "-ERR Can't execute '{}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT are allowed in this context\r\n",
          upper.as_str()
        );
      }
    }

    match upper.as_str() {
      "HELLO" => {
        if tokens.len() > 1 && tokens[1] == "3" {
          self.is_resp3.store(true, Ordering::Release);
          "%1\r\n$5\r\nproto\r\n:3\r\n".to_string()
        } else {
          "-ERR unrecognised hello argument\r\n".to_string()
        }
      }
      "PING" => {
        if self.is_in_sub_mode() {
          "*2\r\n$4\r\npong\r\n$0\r\n\r\n".to_string()
        } else {
          "+PONG\r\n".to_string()
        }
      }
      "SUBSCRIBE" => {
        let mut out = String::with_capacity(tokens[1..].len() * 48);
        let channels = self.subscribed_channels.pin();
        for &ch in &tokens[1..] {
          channels.insert(ch.as_bytes().to_vec());
          self.broker.subscribe(ch.as_bytes(), &self.session);
          let count = self.subscription_count();
          let _ = write!(
            &mut out,
            "*3\r\n$9\r\nsubscribe\r\n${}\r\n{}\r\n:{}\r\n",
            ch.len(),
            ch,
            count
          );
        }
        out
      }
      "PSUBSCRIBE" => {
        let mut out = String::with_capacity(tokens[1..].len() * 48);
        let patterns = self.subscribed_patterns.pin();
        for &pat in &tokens[1..] {
          patterns.insert(pat.as_bytes().to_vec());
          self.broker.pattern_subscribe(pat.as_bytes(), &self.session);
          let count = self.subscription_count();
          let _ = write!(
            &mut out,
            "*3\r\n$10\r\npsubscribe\r\n${}\r\n{}\r\n:{}\r\n",
            pat.len(),
            pat,
            count
          );
        }
        out
      }
      "UNSUBSCRIBE" => {
        let mut out = String::new();
        let channels = self.subscribed_channels.pin();
        if tokens.len() > 1 {
          out.reserve(tokens[1..].len() * 48);
          for &ch in &tokens[1..] {
            channels.remove(ch.as_bytes());
            self.broker.unsubscribe(ch.as_bytes(), &self.session);
            let count = self.subscription_count();
            let _ = write!(
              &mut out,
              "*3\r\n$11\r\nunsubscribe\r\n${}\r\n{}\r\n:{}\r\n",
              ch.len(),
              ch,
              count
            );
          }
        } else {
          let unsubs = self.broker.unsubscribe_all(&self.session);
          if unsubs.is_empty() {
            let null_repr = if self.is_resp3.load(Ordering::Acquire) {
              "_\r\n"
            } else {
              "$-1\r\n"
            };
            let _ = write!(&mut out, "*3\r\n$11\r\nunsubscribe\r\n{}:0\r\n", null_repr);
          } else {
            out.reserve(unsubs.len() * 48);
            for ch in unsubs {
              channels.remove(ch.as_ref());
              let count = self.subscription_count();
              let ch_str = String::from_utf8_lossy(&ch);
              let _ = write!(
                &mut out,
                "*3\r\n$11\r\nunsubscribe\r\n${}\r\n{}\r\n:{}\r\n",
                ch.len(),
                &*ch_str,
                count
              );
            }
          }
        }
        out
      }
      "PUNSUBSCRIBE" => {
        let mut out = String::new();
        let patterns = self.subscribed_patterns.pin();
        if tokens.len() > 1 {
          out.reserve(tokens[1..].len() * 48);
          for &pat in &tokens[1..] {
            patterns.remove(pat.as_bytes());
            self.broker.punsubscribe(pat.as_bytes(), &self.session);
            let count = self.subscription_count();
            let _ = write!(
              &mut out,
              "*3\r\n$12\r\npunsubscribe\r\n${}\r\n{}\r\n:{}\r\n",
              pat.len(),
              pat,
              count
            );
          }
        } else {
          let punsubs = self.broker.punsubscribe_all(&self.session);
          if punsubs.is_empty() {
            let null_repr = if self.is_resp3.load(Ordering::Acquire) {
              "_\r\n"
            } else {
              "$-1\r\n"
            };
            let _ = write!(&mut out, "*3\r\n$12\r\npunsubscribe\r\n{}:0\r\n", null_repr);
          } else {
            out.reserve(punsubs.len() * 48);
            for pat in punsubs {
              patterns.remove(pat.as_ref());
              let count = self.subscription_count();
              let pat_str = String::from_utf8_lossy(&pat);
              let _ = write!(
                &mut out,
                "*3\r\n$12\r\npunsubscribe\r\n${}\r\n{}\r\n:{}\r\n",
                pat.len(),
                &*pat_str,
                count
              );
            }
          }
        }
        out
      }
      "PUBLISH" => {
        if tokens.len() < 3 {
          return "-ERR wrong number of arguments for 'publish' command\r\n".to_string();
        }
        let ch = tokens[1];
        let msg = tokens[2];
        let count = self.broker.publish_now(ch.as_bytes(), msg.as_bytes());

        // RESP3 自发布响应先输出 Push 帧
        let mut prefix = String::new();
        if self.is_resp3.load(Ordering::Acquire) {
          if self.subscribed_channels.pin().contains(ch.as_bytes()) {
            let _ = write!(
              &mut prefix,
              ">3\r\n$7\r\nmessage\r\n${}\r\n{}\r\n${}\r\n{}\r\n",
              ch.len(),
              ch,
              msg.len(),
              msg
            );
          } else {
            let patterns = self.subscribed_patterns.pin();
            for pat in patterns.iter() {
              if glob_match(pat, ch.as_bytes(), false) {
                let pat_str = String::from_utf8_lossy(pat);
                let _ = write!(
                  &mut prefix,
                  ">4\r\n$8\r\npmessage\r\n${}\r\n{}\r\n${}\r\n{}\r\n${}\r\n{}\r\n",
                  pat.len(),
                  &*pat_str,
                  ch.len(),
                  ch,
                  msg.len(),
                  msg
                );
                break;
              }
            }
          }
        }
        format!("{}:{}\r\n", prefix.as_str(), count)
      }
      "SET" => {
        if tokens.len() < 3 {
          "-ERR wrong number of arguments for 'set' command\r\n".to_string()
        } else {
          self
            .kv_store
            .pin()
            .insert(tokens[1].to_string(), tokens[2].to_string());
          "+OK\r\n".to_string()
        }
      }
      "GET" => {
        if tokens.len() < 2 {
          "-ERR wrong number of arguments for 'get' command\r\n".to_string()
        } else {
          let pin = self.kv_store.pin();
          if let Some(val) = pin.get(tokens[1]) {
            format!("${}\r\n{}\r\n", val.len(), val.as_str())
          } else {
            "$-1\r\n".to_string()
          }
        }
      }
      "MULTI" | "QUIT" => "+OK\r\n".to_string(),
      _ => "-ERR unknown command\r\n".to_string(),
    }
  }
}

/// 夹具初始化
pub fn setup() -> Arc<SubscribeBroker> {
  Arc::new(SubscribeBroker::new())
}

/// 辅助发送命令（对标 C# SendCommand 辅助方法，零拼接直接分发）
pub fn send_command(session: &MockSession, args: &[&str]) -> String {
  session.execute_tokens(args)
}

/// 辅助读取响应（对标 C# ReadAvailable 辅助方法）
pub fn read_available(session: &MockSession, cmd: &str) -> String {
  session.execute(cmd)
}

/// 辅助订阅与发布（对标 C# RespPubSubTests.SubscribeAndPublish 夹具逻辑）
pub fn subscribe_and_publish<F>(
  broker: &SubscribeBroker,
  session: &SessionHandle,
  channel: &[u8],
  is_pattern: bool,
  publish_channel: &[u8],
  payload: &[u8],
  on_subscribe: F,
) where
  F: FnOnce(&[u8], &[u8]),
{
  if is_pattern {
    broker.pattern_subscribe(channel, session);
  } else {
    broker.subscribe(channel, session);
  }

  broker.publish_now(publish_channel, payload);
  on_subscribe(publish_channel, payload);
}
