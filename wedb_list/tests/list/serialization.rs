// 对标 C#: libs/server/Objects/List/ListObject.cs / ListObjectImpl.cs
// 序列化语义：Garnet 1:1 二进制格式 (count:i32 + [item_len:i32 + payload]) 往返、
// 损坏数据防御与 bitcode 极速编解码。

use aok::{OK, Void};
use log::info;
use wedb_list::ListObject;

/// 序列化基础往返与跨页规模往返（对标 ListObject.Save/Load 二进制格式）
#[test]
fn serialization_round_trip() -> Void {
  // 基础 3 元素
  let mut list = ListObject::new();
  list.rpush([b"item1", b"item2", b"item3"]);

  let mut buf = Vec::new();
  list.serialize(&mut buf);
  let restored = ListObject::deserialize(&buf)?;
  assert_eq!(restored, list);
  assert_eq!(restored.len(), 3);
  assert_eq!(restored.lindex(1), Some(b"item2".as_slice()));

  // 200 元素跨页规模（页容量 64 -> 多页）
  let mut large = ListObject::new();
  let vals: Vec<Vec<u8>> = (0..200)
    .map(|i| format!("element_{i:04}").into_bytes())
    .collect();
  large.rpush(vals.iter().map(Vec::as_slice));

  let mut large_buf = Vec::new();
  large.serialize(&mut large_buf);
  let large_restored = ListObject::deserialize(&large_buf)?;
  assert_eq!(large_restored, large);
  assert_eq!(large_restored.lindex(0), Some(b"element_0000".as_slice()));
  assert_eq!(large_restored.lindex(199), Some(b"element_0199".as_slice()));
  assert_eq!(large_restored.lindex(-1), Some(b"element_0199".as_slice()));

  info!("序列化往返语义通过：Garnet 1:1 格式跨页无损");
  OK
}

/// 500 元素跨页往返与 lset 跨页覆写（转写新增分页结构回归）
#[test]
fn paged_serialization_round_trip() -> Void {
  let expect: Vec<Vec<u8>> = (0..500).map(|i| format!("e{i:03}").into_bytes()).collect();
  let mut list = ListObject::new();
  list.rpush(expect.iter().map(Vec::as_slice));

  // Garnet 1:1 格式
  let mut buf = Vec::new();
  list.serialize(&mut buf);
  let restored = ListObject::deserialize(&buf)?;
  assert_eq!(restored, list);
  assert_eq!(restored.lrange(0, -1), expect);

  // bitcode 格式
  let bc_restored = ListObject::from_bitcode(&list.to_bitcode())?;
  assert_eq!(bc_restored, list);

  // lset 跨页覆写（64/192 分别落在不同页）
  let mut m = list.clone();
  m.lset(64, b"MOD")?;
  m.lset(192, b"MOD")?;
  let mut e = expect.clone();
  e[64] = b"MOD".to_vec();
  e[192] = b"MOD".to_vec();
  assert_eq!(m.lrange(0, -1), e);

  info!("paged_serialization_round_trip 语义通过：跨页序列化与覆写");
  OK
}

/// 反序列化错误路径：截断、负 count、声明数超容、负长度与载荷截断（转写新增）
#[test]
fn deserialize_error_paths() -> Void {
  use wedb_list::Error;

  // 缓冲区过短
  assert_eq!(ListObject::deserialize(&[]), Err(Error::BufferTooShort));
  assert_eq!(
    ListObject::deserialize(&[1, 2, 3]),
    Err(Error::BufferTooShort)
  );

  // 负 count
  let mut buf = (-1i32).to_le_bytes().to_vec();
  assert_eq!(ListObject::deserialize(&buf), Err(Error::CorruptedData));

  // count 声明大于实际可容纳元素数（防恶意大数预分配）
  buf.clear();
  buf.extend_from_slice(&10i32.to_le_bytes());
  assert_eq!(ListObject::deserialize(&buf), Err(Error::CorruptedData));

  // 元素长度为负
  buf.clear();
  buf.extend_from_slice(&1i32.to_le_bytes());
  buf.extend_from_slice(&(-2i32).to_le_bytes());
  assert_eq!(ListObject::deserialize(&buf), Err(Error::CorruptedData));

  // 元素载荷截断
  buf.clear();
  buf.extend_from_slice(&1i32.to_le_bytes());
  buf.extend_from_slice(&5i32.to_le_bytes());
  buf.extend_from_slice(b"abc");
  assert_eq!(ListObject::deserialize(&buf), Err(Error::BufferTooShort));

  // 良好输入：2 个空元素
  let mut ok_buf = Vec::new();
  ok_buf.extend_from_slice(&2i32.to_le_bytes());
  ok_buf.extend_from_slice(&0i32.to_le_bytes());
  ok_buf.extend_from_slice(&0i32.to_le_bytes());
  let restored = ListObject::deserialize(&ok_buf)?;
  assert_eq!(restored.len(), 2);
  assert_eq!(restored.lindex(0), Some([].as_slice()));
  assert_eq!(restored.lindex(1), Some([].as_slice()));

  info!("deserialize_error_paths 语义通过：损坏输入全路径防御");
  OK
}

/// bitcode 极速编解码：空列表、二进制载荷、大规模与非法数据防御（转写新增）
#[test]
fn bitcode_serialization() -> Void {
  // 空列表
  let empty = ListObject::new();
  let restored_empty = ListObject::from_bitcode(&empty.to_bitcode())?;
  assert_eq!(restored_empty, empty);
  assert_eq!(restored_empty.len(), 0);

  // 含空元素与二进制载荷
  let mut list = ListObject::new();
  list.rpush([
    b"apple".to_vec(),
    b"".to_vec(),
    b"\x00\xffbinary\xfe".to_vec(),
  ]);
  let restored = ListObject::from_bitcode(&list.to_bitcode())?;
  assert_eq!(restored, list);
  assert_eq!(restored.lindex(0), Some(b"apple".as_slice()));
  assert_eq!(restored.lindex(1), Some(b"".as_slice()));
  assert_eq!(restored.lindex(2), Some(b"\x00\xffbinary\xfe".as_slice()));

  // 大规模元素
  let mut large = ListObject::new();
  let vals: Vec<Vec<u8>> = (0..500)
    .map(|i| format!("val_{i:05}").into_bytes())
    .collect();
  large.rpush(vals);
  let large_restored = ListObject::from_bitcode(&large.to_bitcode())?;
  assert_eq!(large_restored.len(), 500);
  assert_eq!(large_restored.lindex(0), Some(b"val_00000".as_slice()));
  assert_eq!(large_restored.lindex(499), Some(b"val_00499".as_slice()));

  // 非法数据防御
  assert!(ListObject::from_bitcode(&[0xff, 0xff, 0xff]).is_err());

  info!("bitcode_serialization 语义通过：极速编解码与防御");
  OK
}

/// 序列化全前缀截断鲁棒性：任意字节截断必须安全报错绝不恐慌（转写新增）
#[test]
fn serialization_truncation_robustness() -> Void {
  // 构建包含空元素、单元素、长二进制载荷的列表
  let mut list = ListObject::new();
  list.rpush([
    b"".to_vec(),
    b"A".to_vec(),
    vec![0x00, 0xff, 0xfe, 0x01, 0x42],
    vec![b'x'; 2048],
  ]);

  // Garnet 1:1 格式全字节前缀截断
  let mut garnet_buf = Vec::new();
  list.serialize(&mut garnet_buf);
  for cut in 0..garnet_buf.len() {
    let res = ListObject::deserialize(&garnet_buf[..cut]);
    assert!(
      res.is_err(),
      "Garnet 格式截断于 {cut}/{} 应安全报错",
      garnet_buf.len()
    );
  }
  assert_eq!(ListObject::deserialize(&garnet_buf)?, list);

  // bitcode 格式全字节前缀截断
  let bitcode_buf = list.to_bitcode();
  for cut in 0..bitcode_buf.len() {
    let res = ListObject::from_bitcode(&bitcode_buf[..cut]);
    assert!(
      res.is_err(),
      "bitcode 截断于 {cut}/{} 应安全报错",
      bitcode_buf.len()
    );
  }
  assert_eq!(ListObject::from_bitcode(&bitcode_buf)?, list);

  // 超大载荷 (100KB)
  let mut large = ListObject::new();
  large.rpush([vec![0xab; 1024 * 100]]);
  let large_restored = ListObject::from_bitcode(&large.to_bitcode())?;
  assert_eq!(large_restored, large);
  assert_eq!(large_restored.lindex(0).unwrap().len(), 1024 * 100);

  info!("serialization_truncation_robustness 语义通过：任意截断安全报错");
  OK
}
