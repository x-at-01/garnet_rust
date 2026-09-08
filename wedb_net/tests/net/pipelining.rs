use std::sync::Arc;

use aok::{OK, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  runtime::spawn,
};
use log::info;

use crate::support::{
  DEFAULT_BUF_CAPACITY, NetworkTestFixture, send_and_recv, send_and_recv_exact,
};

/// 单批次流水线测试命令数量
const PIPELINE_BATCH_SIZE: usize = 10;
/// 单个 +OK\r\n 响应报文字节数
const OK_RESP_LEN: usize = b"+OK\r\n".len();
/// 批量流水线预期收到的总字节数（50 字节）
const PIPELINE_BATCH_EXPECTED_LEN: usize = PIPELINE_BATCH_SIZE * OK_RESP_LEN;

/// 粘包测试写出的单报文三命令请求
const STICKY_CMD: &[u8] = b"*3\r\n$3\r\nSET\r\n$2\r\ns1\r\n$2\r\nv1\r\n*3\r\n$3\r\nSET\r\n$2\r\ns2\r\n$2\r\nv2\r\n*2\r\n$3\r\nGET\r\n$2\r\ns1\r\n";
/// 粘包测试预期的完整响应
const EXPECTED_STICKY_RESP: &[u8] = b"+OK\r\n+OK\r\n$2\r\nv1\r\n";

/// 拆包分段测试的第一段半包数据
const SPLIT_PART1: &[u8] = b"*3\r\n$3\r\nSET\r\n$5\r\nsp";
/// 拆包分段测试的第二段半包数据
const SPLIT_PART2: &[u8] = b"lit\r\n$3\r\nval\r\n";

/// 洪泛测试命令总数
const FLOOD_COUNT: usize = 600;

/// 混合空协议帧的复杂流水线全套报文（纯编译期常量组装）
const MIXED_PIPELINE: &[u8] = b"\r\n\r\n*0\r\n*-1\r\n\
PING\r\n\
*3\r\n$3\r\nSET\r\n$2\r\nk1\r\n$2\r\nv1\r\n\
*0\r\n\r\n\
MULTI\r\n\
*-1\r\n\r\n\
*3\r\n$3\r\nSET\r\n$2\r\nk2\r\n$2\r\nv2\r\n\
*0\r\n\
*2\r\n$3\r\nGET\r\n$2\r\nk1\r\n\
\r\n\
EXEC\r\n\
*2\r\n$3\r\nGET\r\n$2\r\nk2\r\n\
*0\r\n\r\n";

/// 混合空协议帧复杂流水线预期的完整回包
const EXPECTED_MIXED_RESP: &[u8] =
  b"+PONG\r\n+OK\r\n+OK\r\n+QUEUED\r\n+QUEUED\r\n*2\r\n+OK\r\n$2\r\nv1\r\n$2\r\nv2\r\n";

/// 并发客户端数量
const CONCURRENCY: usize = 8;
/// 每个客户端执行的流水线操作轮数
const OPS_PER_CLIENT: usize = 20;

/// 批量流水线请求测试
/// 验证客户端一次性批量写出多条命令时，服务端能按顺序依次处理并流水线回传响应。
#[compio::test]
async fn test_pipelining_requests() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 组装批量流水线指令（10 条命令单次报文写出）
  let mut pipeline_req = Vec::new();
  for i in 0..PIPELINE_BATCH_SIZE {
    let k = format!("p{}", i);
    let v = format!("v{}", i);
    let cmd = format!(
      "*3\r\n$3\r\nSET\r\n${}\r\n{}\r\n${}\r\n{}\r\n",
      k.len(),
      k.as_str(),
      v.len(),
      v.as_str()
    );
    pipeline_req.extend_from_slice(cmd.as_bytes());
  }

  // 复用零分配缓冲读取流，验证 10 条确认响应报文按序返回
  let received =
    send_and_recv_exact(&mut client, &pipeline_req, PIPELINE_BATCH_EXPECTED_LEN).await?;
  assert_eq!(received.len(), PIPELINE_BATCH_EXPECTED_LEN);
  for chunk in received.as_chunks::<5>().0 {
    assert_eq!(chunk, b"+OK\r\n");
  }

  info!("网络流水线批处理测试通过");
  OK
}

/// 网络粘包与拆包流式解析测试
/// 验证单包多命令粘包解析，以及单命令分包切断时的半包流式零拷贝拼接。
#[compio::test]
async fn test_framing_sticky_and_split() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 粘包测试：单次网络写出包含三条流水线命令，使用 send_and_recv_exact 精确回收
  let received = send_and_recv_exact(&mut client, STICKY_CMD, EXPECTED_STICKY_RESP.len()).await?;
  assert_eq!(received, EXPECTED_STICKY_RESP);

  // 2. 拆包测试：单条命令分两次发送验证半包流式拼接
  let (w1, _) = client.write_all(SPLIT_PART1.to_vec()).await.into();
  w1?;
  let (w2, _) = client.write_all(SPLIT_PART2.to_vec()).await.into();
  w2?;

  let buf = Vec::with_capacity(DEFAULT_BUF_CAPACITY);
  let BufResult(read_res, buf) = client.read(buf).await;
  let n = read_res?;
  assert_eq!(&buf[..n], b"+OK\r\n");

  info!("粘包与半包拆包零拷贝拼接验证通过");
  OK
}

/// 单连接流水线洪泛（600 条命令单包写出）
/// 覆盖接收缓冲扩容、内层批量解析循环、发送缓冲聚合与写出通道搬运的全链路零丢失。
#[compio::test]
async fn test_pipelined_flood_no_loss() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  let req = b"PING\r\n".repeat(FLOOD_COUNT);
  let expected = b"+PONG\r\n".repeat(FLOOD_COUNT);
  let resp = send_and_recv_exact(&mut client, &req, expected.len()).await?;
  assert_eq!(resp, expected);

  // 洪泛后会话保持健康
  let resp = send_and_recv(&mut client, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  info!("流水线洪泛零丢失验证通过");
  OK
}

/// 混合空协议帧的复杂事务流水线请求单包发送验证
/// 前导、穿插及尾随空协议帧（\r\n, *0\r\n, *-1\r\n）混合 MULTI 事务、数据写入与读取命令，
/// 验证流水线单次写出无漏包、无错位、事务内空帧不导致 EXECABORT。
#[compio::test]
async fn test_pipelining_with_mixed_empty_frames_and_transaction() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  let resp = send_and_recv_exact(&mut client, MIXED_PIPELINE, EXPECTED_MIXED_RESP.len()).await?;
  assert_eq!(resp, EXPECTED_MIXED_RESP);

  // 会话依然健康
  let resp = send_and_recv(&mut client, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  info!("混合空帧复杂事务流水线验证通过");
  OK
}

/// 多并发客户端高频吞吐与管道稳定性测试
/// 8 个并发客户端并行高频发送流水线与交互请求，验证高并发网络调度与内存安全。
#[compio::test]
async fn test_concurrent_clients_throughput() -> Void {
  let fixture = Arc::new(NetworkTestFixture::setup().await?);
  let mut handles = Vec::with_capacity(CONCURRENCY);

  for c in 0..CONCURRENCY {
    let fix = Arc::clone(&fixture);
    let handle = spawn(async move {
      let mut client = fix.connect_client().await?;
      for i in 0..OPS_PER_CLIENT {
        let k = format!("k_{}_{}", c, i);
        let v = format!("v_{}_{}", c, i);
        let set_cmd = format!(
          "*3\r\n$3\r\nSET\r\n${}\r\n{}\r\n${}\r\n{}\r\n",
          k.len(),
          k.as_str(),
          v.len(),
          v.as_str()
        );
        let resp = send_and_recv(&mut client, set_cmd.as_bytes()).await?;
        assert_eq!(resp, b"+OK\r\n");

        let get_cmd = format!("*2\r\n$3\r\nGET\r\n${}\r\n{}\r\n", k.len(), k.as_str());
        let resp = send_and_recv(&mut client, get_cmd.as_bytes()).await?;
        let expected = format!("${}\r\n{}\r\n", v.len(), v.as_str());
        assert_eq!(resp, expected.as_bytes());
      }
      OK
    });
    handles.push(handle);
  }

  for h in handles {
    h.await.unwrap()?;
  }

  info!("多客户端并发高频吞吐验证通过");
  OK
}
