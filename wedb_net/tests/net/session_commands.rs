use aok::{OK, Void};

use crate::support::{NetworkTestFixture, send_and_recv};

/// 会话 SELECT 逻辑数据库命名空间隔离测试
/// 键命名空间前缀由 StoreSession 的 active_db 原子变量承载：切换数据库后
/// 同名键相互不可见，切回原库原值完好；非法库号被拒绝且不改变当前库。
#[compio::test]
async fn test_select_database_namespace_isolation() -> Void {
  const SET_K_V1: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$2\r\nv1\r\n";
  const SET_K_V2: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$2\r\nv2\r\n";
  const GET_K: &[u8] = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";

  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 默认库（db 0）写入基准值
  assert_eq!(send_and_recv(&mut client, SET_K_V1).await?, b"+OK\r\n");
  assert_eq!(send_and_recv(&mut client, GET_K).await?, b"$2\r\nv1\r\n");

  // 2. 切换到 db 1：同名键不可见，写入独立值
  assert_eq!(
    send_and_recv(&mut client, b"SELECT 1\r\n").await?,
    b"+OK\r\n"
  );
  assert_eq!(send_and_recv(&mut client, GET_K).await?, b"$-1\r\n");
  assert_eq!(send_and_recv(&mut client, SET_K_V2).await?, b"+OK\r\n");
  assert_eq!(send_and_recv(&mut client, GET_K).await?, b"$2\r\nv2\r\n");

  // 3. 回到 db 0：原值完好，未被 db 1 覆盖（命名空间相互隔离）
  assert_eq!(
    send_and_recv(&mut client, b"SELECT 0\r\n").await?,
    b"+OK\r\n"
  );
  assert_eq!(send_and_recv(&mut client, GET_K).await?, b"$2\r\nv1\r\n");

  // 4. 非法库号被拒绝且不改变当前库
  assert!(
    send_and_recv(&mut client, b"SELECT abc\r\n")
      .await?
      .starts_with(b"-ERR")
  );
  assert!(
    send_and_recv(&mut client, b"SELECT -1\r\n")
      .await?
      .starts_with(b"-ERR")
  );
  assert_eq!(send_and_recv(&mut client, GET_K).await?, b"$2\r\nv1\r\n");

  OK
}

/// 列表弹出命令 count 参数语义测试（对齐 Redis LPOP/RPOP key [count]）
/// count=0 返回空数组、负数/非整数被拒绝且连接保持、带 count 返回数组、无 count 返回单元素。
#[compio::test]
async fn test_pop_count_semantics() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 灌入列表 [a, b, c]
  assert_eq!(
    send_and_recv(
      &mut client,
      b"*5\r\n$5\r\nRPUSH\r\n$1\r\nl\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n"
    )
    .await?,
    b":3\r\n"
  );

  // 2. count=0 合法：返回空数组，列表不受影响
  assert_eq!(
    send_and_recv(&mut client, b"*3\r\n$4\r\nLPOP\r\n$1\r\nl\r\n$1\r\n0\r\n").await?,
    b"*0\r\n"
  );
  assert_eq!(
    send_and_recv(&mut client, b"*2\r\n$4\r\nLLEN\r\n$1\r\nl\r\n").await?,
    b":3\r\n"
  );

  // 3. 负数与非整数 count 被拒绝（不消费元素，连接保持）
  assert!(
    send_and_recv(&mut client, b"*3\r\n$4\r\nRPOP\r\n$1\r\nl\r\n$2\r\n-1\r\n")
      .await?
      .starts_with(b"-ERR")
  );
  assert!(
    send_and_recv(&mut client, b"*3\r\n$4\r\nRPOP\r\n$1\r\nl\r\n$3\r\nabc\r\n")
      .await?
      .starts_with(b"-ERR")
  );
  assert_eq!(
    send_and_recv(&mut client, b"*2\r\n$4\r\nLLEN\r\n$1\r\nl\r\n").await?,
    b":3\r\n"
  );

  // 4. 带 count 弹出：返回数组
  assert_eq!(
    send_and_recv(&mut client, b"*3\r\n$4\r\nRPOP\r\n$1\r\nl\r\n$1\r\n2\r\n").await?,
    b"*2\r\n$1\r\nc\r\n$1\r\nb\r\n"
  );

  // 5. 无 count 弹出：返回单元素；弹空后返回 nil
  assert_eq!(
    send_and_recv(&mut client, b"*2\r\n$4\r\nLPOP\r\n$1\r\nl\r\n").await?,
    b"$1\r\na\r\n"
  );
  assert_eq!(
    send_and_recv(&mut client, b"*2\r\n$4\r\nLPOP\r\n$1\r\nl\r\n").await?,
    b"$-1\r\n"
  );

  OK
}

/// ZADD 选项语法测试（NX/XX/GT/LT/CH，对标 Redis ZADD 选项语义）
#[compio::test]
async fn test_zadd_options() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 基础添加返回新增数
  assert_eq!(
    send_and_recv(
      &mut client,
      b"*4\r\n$4\r\nZADD\r\n$1\r\nz\r\n$3\r\n1.5\r\n$1\r\nm\r\n"
    )
    .await?,
    b":1\r\n"
  );

  // 2. NX 对已存在成员不更新：返回 0，分数保持
  assert_eq!(
    send_and_recv(
      &mut client,
      b"*5\r\n$4\r\nZADD\r\n$1\r\nz\r\n$2\r\nNX\r\n$3\r\n9.9\r\n$1\r\nm\r\n"
    )
    .await?,
    b":0\r\n"
  );
  assert_eq!(
    send_and_recv(&mut client, b"*3\r\n$6\r\nZSCORE\r\n$1\r\nz\r\n$1\r\nm\r\n").await?,
    b"$3\r\n1.5\r\n"
  );

  // 3. GT 仅更大分数更新：较小分数不更新返回 0；更大分数更新成功但无 CH 仍计新增数 0
  assert_eq!(
    send_and_recv(
      &mut client,
      b"*5\r\n$4\r\nZADD\r\n$1\r\nz\r\n$2\r\nGT\r\n$3\r\n0.5\r\n$1\r\nm\r\n"
    )
    .await?,
    b":0\r\n"
  );
  assert_eq!(
    send_and_recv(
      &mut client,
      b"*5\r\n$4\r\nZADD\r\n$1\r\nz\r\n$2\r\nGT\r\n$3\r\n2.5\r\n$1\r\nm\r\n"
    )
    .await?,
    b":0\r\n"
  );
  assert_eq!(
    send_and_recv(&mut client, b"*3\r\n$6\r\nZSCORE\r\n$1\r\nz\r\n$1\r\nm\r\n").await?,
    b"$3\r\n2.5\r\n"
  );

  // 4. CH 统计变更总数（新增 + 更新）
  assert_eq!(
    send_and_recv(
      &mut client,
      b"*7\r\n$4\r\nZADD\r\n$1\r\nz\r\n$2\r\nCH\r\n$3\r\n3.5\r\n$1\r\nm\r\n$1\r\n1\r\n$1\r\nn\r\n"
    )
    .await?,
    b":2\r\n"
  );

  // 5. XX 对不存在成员不新增：返回 0
  assert_eq!(
    send_and_recv(
      &mut client,
      b"*5\r\n$4\r\nZADD\r\n$1\r\nz\r\n$2\r\nXX\r\n$1\r\n1\r\n$1\r\nq\r\n"
    )
    .await?,
    b":0\r\n"
  );

  // 6. 非法选项与奇偶错位参数报语法错误，连接保持
  assert!(
    send_and_recv(
      &mut client,
      b"*4\r\n$4\r\nZADD\r\n$1\r\nz\r\n$4\r\nBADX\r\n$1\r\nm\r\n"
    )
    .await?
    .starts_with(b"-ERR")
  );
  assert_eq!(
    send_and_recv(&mut client, b"*2\r\n$5\r\nZCARD\r\n$1\r\nz\r\n").await?,
    b":2\r\n"
  );

  OK
}

/// MGET 批量读路径回归（底层 12 项预取流水线批量读 mget_each）
///
/// 结果顺序恒等于请求顺序：命中返回值、缺失与错型（哈希类记录）均返回 nil；
/// 混合批次一次回报完整数组。
#[compio::test]
async fn test_mget_batch_semantics() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 灌入字符串键与哈希键（错型样本）
  assert_eq!(
    send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nm1\r\n$1\r\na\r\n").await?,
    b"+OK\r\n"
  );
  assert_eq!(
    send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nm2\r\n$2\r\nbb\r\n").await?,
    b"+OK\r\n"
  );
  assert_eq!(
    send_and_recv(
      &mut client,
      b"*4\r\n$4\r\nHSET\r\n$2\r\nmh\r\n$1\r\nf\r\n$1\r\nv\r\n"
    )
    .await?,
    b":1\r\n"
  );

  // 2. 混合批次：命中、缺失、错型一次回报（顺序对位）
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$4\r\nMGET\r\n$2\r\nm1\r\n$7\r\nmissing\r\n$2\r\nm2\r\n$2\r\nmh\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(
    resp,
    b"*5\r\n$1\r\na\r\n$-1\r\n$2\r\nbb\r\n$-1\r\n$1\r\na\r\n"
  );

  // 3. 空参数被拒绝
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nMGET\r\n").await?;
  assert_eq!(
    resp,
    b"-ERR wrong number of arguments for 'mget' command\r\n"
  );

  OK
}
