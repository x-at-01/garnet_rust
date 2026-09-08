//! GEO 系列扩展命令、集合存储运算、零拷贝借用查询及 Bitcode 序列化集成测试
use aok::{OK, Void};
use coarsetime::Clock;
use wedb_zset::{
  CompactZSet, CompactZSetExt, ExpireOpt, ExpireResult, GeoDistanceUnit, GeoOrder, GeoOrigin,
  GeoSearchOpt, GeoShape, SortedSetAggregate, SortedSetObject, ZAddOpt,
};

/// 1. GEOSEARCH: 半径与矩形窗口、坐标与成员原点、距离/坐标/哈希展示与排序/截断
#[test]
fn test_geosearch_radius_and_box() -> Void {
  let mut z = SortedSetObject::new();
  // 西西里岛三座城市坐标 (lat, lon)
  // Palermo: 38.115556, 13.361389
  // Catania: 37.502669, 15.087269
  // Agrigento: 37.316667, 13.583333
  z.geoadd(38.115556, 13.361389, b"Palermo")?;
  z.geoadd(37.502669, 15.087269, b"Catania")?;
  z.geoadd(37.316667, 13.583333, b"Agrigento")?;

  // 1.1 从 Palermo 出发，半径 200 km 内搜索 (按距离升序，附带距离与坐标)
  let res = z.geosearch(
    GeoOrigin::Member(b"Palermo".to_vec()),
    GeoShape::ByRadius {
      radius: 200.0,
      unit: GeoDistanceUnit::KM,
    },
    GeoSearchOpt {
      order: GeoOrder::Asc,
      with_dist: true,
      with_coord: true,
      with_hash: true,
      ..GeoSearchOpt::default()
    },
  )?;

  assert_eq!(res.len(), 3);
  assert_eq!(res[0].member, b"Palermo");
  assert!(res[0].dist.unwrap() < 1.0); // 距离约 0
  assert!(res[0].hash.is_some());
  assert!(res[0].coord.is_some());

  // 第二近是 Agrigento (~90km)
  assert_eq!(res[1].member, b"Agrigento");
  let dist_agr = res[1].dist.unwrap();
  assert!(dist_agr > 85.0 && dist_agr < 100.0);

  // 最远是 Catania (~166km)
  assert_eq!(res[2].member, b"Catania");
  let dist_cat = res[2].dist.unwrap();
  assert!(dist_cat > 160.0 && dist_cat < 175.0);

  // 1.2 降序排序验证 (DESC)
  let desc_res = z.geosearch(
    GeoOrigin::Member(b"Palermo".to_vec()),
    GeoShape::ByRadius {
      radius: 200.0,
      unit: GeoDistanceUnit::KM,
    },
    GeoSearchOpt {
      order: GeoOrder::Desc,
      with_dist: true,
      ..GeoSearchOpt::default()
    },
  )?;
  assert_eq!(desc_res[0].member, b"Catania");
  assert_eq!(desc_res[2].member, b"Palermo");

  // 1.3 COUNT 截断与 ANY
  let count_res = z.geosearch(
    GeoOrigin::Member(b"Palermo".to_vec()),
    GeoShape::ByRadius {
      radius: 200.0,
      unit: GeoDistanceUnit::KM,
    },
    GeoSearchOpt {
      order: GeoOrder::Asc,
      count: Some(2),
      with_dist: true,
      ..GeoSearchOpt::default()
    },
  )?;
  assert_eq!(count_res.len(), 2);
  assert_eq!(count_res[0].member, b"Palermo");
  assert_eq!(count_res[1].member, b"Agrigento");

  // 1.4 矩形区域搜索 (ByBox): 宽 100km, 高 100km
  let box_res = z.geosearch(
    GeoOrigin::Coord {
      lon: 13.361389,
      lat: 38.115556,
    },
    GeoShape::ByBox {
      width: 100.0,
      height: 100.0,
      unit: GeoDistanceUnit::KM,
    },
    GeoSearchOpt {
      order: GeoOrder::Asc,
      with_dist: true,
      ..GeoSearchOpt::default()
    },
  )?;
  // 100x100km 矩形框仅包含 Palermo (Agrigento 纬度相差 ~88km 超出高 50km 半宽)
  assert_eq!(box_res.len(), 1);
  assert_eq!(box_res[0].member, b"Palermo");

  OK
}

/// 2. GEOSEARCHSTORE: 将搜索结果写入目标 ZSet，支持存储 GeoHash 或距离
#[test]
fn test_geosearch_store() -> Void {
  let mut z = SortedSetObject::new();
  z.geoadd(38.115556, 13.361389, b"Palermo")?;
  z.geoadd(37.502669, 15.087269, b"Catania")?;
  z.geoadd(37.316667, 13.583333, b"Agrigento")?;

  // 2.1 存储 GeoHash (store_dist = false)
  let mut dest_hash = SortedSetObject::new();
  let count = z.geosearch_store(
    &mut dest_hash,
    GeoOrigin::Member(b"Palermo".to_vec()),
    GeoShape::ByRadius {
      radius: 200.0,
      unit: GeoDistanceUnit::KM,
    },
    GeoSearchOpt {
      order: GeoOrder::Asc,
      ..GeoSearchOpt::default()
    },
    false,
  )?;
  assert_eq!(count, 3);
  assert_eq!(dest_hash.len(), 3);
  // 分值应为对应的 52 位 GeoHash 整数编码
  let palermo_score = dest_hash.zscore(b"Palermo").unwrap();
  assert!(palermo_score > 1e14); // 52 位整数转换成浮点数

  // 2.2 存储距离 (store_dist = true, 单位为 KM)
  let mut dest_dist = SortedSetObject::new();
  let count = z.geosearch_store(
    &mut dest_dist,
    GeoOrigin::Member(b"Palermo".to_vec()),
    GeoShape::ByRadius {
      radius: 200.0,
      unit: GeoDistanceUnit::KM,
    },
    GeoSearchOpt {
      order: GeoOrder::Asc,
      ..GeoSearchOpt::default()
    },
    true,
  )?;
  assert_eq!(count, 3);
  let palermo_dist = dest_dist.zscore(b"Palermo").unwrap();
  let agrigento_dist = dest_dist.zscore(b"Agrigento").unwrap();
  let catania_dist = dest_dist.zscore(b"Catania").unwrap();
  assert!(palermo_dist < 1.0);
  assert!(agrigento_dist > 85.0 && agrigento_dist < 100.0);
  assert!(catania_dist > 160.0 && catania_dist < 175.0);

  OK
}

/// 3. GEORADIUS 与 GEORADIUSBYMEMBER 语义效验
#[test]
fn test_georadius_and_georadiusbymember() -> Void {
  let mut z = SortedSetObject::new();
  z.geoadd(38.115556, 13.361389, b"Palermo")?;
  z.geoadd(37.502669, 15.087269, b"Catania")?;
  z.geoadd(37.316667, 13.583333, b"Agrigento")?;

  // GEORADIUS: 经纬度原点
  let items = z.georadius(
    15.0,
    37.5,
    100.0,
    GeoDistanceUnit::KM,
    GeoSearchOpt {
      order: GeoOrder::Asc,
      with_dist: true,
      with_coord: true,
      ..GeoSearchOpt::default()
    },
  )?;
  assert_eq!(items.len(), 1);
  assert_eq!(items[0].member, b"Catania");

  // GEORADIUSBYMEMBER: 成员原点
  let items = z.georadiusbymember(
    b"Catania",
    200.0,
    GeoDistanceUnit::KM,
    GeoSearchOpt {
      order: GeoOrder::Asc,
      count: Some(2),
      with_dist: true,
      with_hash: true,
      ..GeoSearchOpt::default()
    },
  )?;
  assert_eq!(items.len(), 2);
  assert_eq!(items[0].member, b"Catania");
  assert_eq!(items[1].member, b"Agrigento");

  OK
}

/// 4. 集合存储运算: ZUNIONSTORE / ZINTERSTORE / ZDIFFSTORE
#[test]
fn test_zset_store_operations() -> Void {
  let mut z1 = SortedSetObject::new();
  z1.zadd(1.0, b"one", ZAddOpt::default())?;
  z1.zadd(2.0, b"two", ZAddOpt::default())?;

  let mut z2 = SortedSetObject::new();
  z2.zadd(10.0, b"one", ZAddOpt::default())?;
  z2.zadd(20.0, b"two", ZAddOpt::default())?;
  z2.zadd(30.0, b"three", ZAddOpt::default())?;

  // 4.1 ZUNIONSTORE 默认权重 1.0，默认 SUM 聚合
  let mut union_dest = SortedSetObject::new();
  let count = union_dest.zunionstore(&[&z1, &z2], None, SortedSetAggregate::Sum)?;
  assert_eq!(count, 3);
  assert_eq!(union_dest.zscore(b"one").unwrap(), 11.0);
  assert_eq!(union_dest.zscore(b"two").unwrap(), 22.0);
  assert_eq!(union_dest.zscore(b"three").unwrap(), 30.0);

  // 4.2 ZINTERSTORE 带加权与 MAX 聚合
  let mut inter_dest = SortedSetObject::new();
  let count = inter_dest.zinterstore(
    &[&z1, &z2],
    Some(&[2.0, 0.5]), // one: max(1*2, 10*0.5)=5; two: max(2*2, 20*0.5)=10
    SortedSetAggregate::Max,
  )?;
  assert_eq!(count, 2);
  assert_eq!(inter_dest.zscore(b"one").unwrap(), 5.0);
  assert_eq!(inter_dest.zscore(b"two").unwrap(), 10.0);
  assert!(inter_dest.zscore(b"three").is_none());

  // 4.3 ZDIFFSTORE
  let mut diff_dest = SortedSetObject::new();
  let count = diff_dest.zdiffstore(&[&z2, &z1])?;
  assert_eq!(count, 1);
  assert_eq!(diff_dest.zscore(b"three").unwrap(), 30.0);
  assert!(diff_dest.zscore(b"one").is_none());

  OK
}

/// 5. 零拷贝借用切片查询与单元素 Pop 接口
#[test]
fn test_borrowed_slice_queries_and_pop() -> Void {
  let mut z = SortedSetObject::new();
  for i in 1..=5 {
    let mut member = String::from("item");
    let mut ibuf = itoa::Buffer::new();
    member.push_str(ibuf.format(i));
    z.zadd(i as f64, member.into_bytes(), ZAddOpt::default())?;
  }

  // 5.1 zrange_borrowed 零拷贝切片
  let borrowed = z.zrange_borrowed(0, 2, false);
  assert_eq!(borrowed.len(), 3);
  assert_eq!(borrowed[0], (b"item1".as_slice(), 1.0));
  assert_eq!(borrowed[1], (b"item2".as_slice(), 2.0));
  assert_eq!(borrowed[2], (b"item3".as_slice(), 3.0));

  // 5.2 zrevrange_borrowed
  let rev_borrowed = z.zrevrange_borrowed(0, 1);
  assert_eq!(rev_borrowed.len(), 2);
  assert_eq!(rev_borrowed[0], (b"item5".as_slice(), 5.0));
  assert_eq!(rev_borrowed[1], (b"item4".as_slice(), 4.0));

  // 5.3 zscan_borrowed 游标遍历
  let (next_cursor, scanned) = z.zscan_borrowed(0, 10, None);
  assert_eq!(next_cursor, 0);
  assert_eq!(scanned.len(), 5);

  // 5.4 pop_min / pop_max
  let min_popped = z.pop_min();
  assert_eq!(min_popped, Some((b"item1".to_vec(), 1.0)));
  assert_eq!(z.len(), 4);

  let max_popped = z.pop_max();
  assert_eq!(max_popped, Some((b"item5".to_vec(), 5.0)));
  assert_eq!(z.len(), 3);

  OK
}

/// 6. 完备成员 TTL 体系: ZPEXPIRE / ZEXPIREAT / ZTTL / ZPTTL / ZEXPIRETIME / ZPERSIST
#[test]
fn test_member_ttl_operations() -> Void {
  let mut z = SortedSetObject::new();
  z.zadd(10.0, b"key1", ZAddOpt::default())?;

  let now = Clock::now_since_epoch().as_millis();
  let expire_at_ms = now + 50_000;
  let expire_at_sec = (expire_at_ms / 1000) as i64;

  // 6.1 zpexpireat 毫秒绝对时间戳设置
  assert_eq!(
    z.zpexpireat(b"key1", expire_at_ms, ExpireOpt::default()),
    ExpireResult::Ok
  );

  // 6.2 zpttl 与 zttl 查询
  let pttl = z.zpttl(b"key1");
  assert!(pttl > 40_000 && pttl <= 50_000);

  // 6.3 zexpiretime (秒) 与 zpexpiretime (毫秒) 查询
  let exp_sec = z.zexpiretime(b"key1");
  assert_eq!(exp_sec, expire_at_sec);
  let exp_ms = z.zpexpiretime(b"key1");
  assert_eq!(exp_ms, expire_at_ms as i64);

  // 6.4 zpersist 清除 TTL
  assert!(z.zpersist(b"key1"));
  assert_eq!(z.zttl(b"key1"), -1);
  assert_eq!(z.zexpiretime(b"key1"), -1);

  // 6.5 未存在成员查询返回 -2
  assert_eq!(z.zttl(b"nonexistent"), -2);
  assert_eq!(z.zexpiretime(b"nonexistent"), -2);

  // 6.6 zpexpire 相对毫秒
  assert_eq!(
    z.zpexpire(b"key1", 30_000, ExpireOpt::default()),
    ExpireResult::Ok
  );
  assert!(z.zpttl(b"key1") > 20_000);

  OK
}

/// 7. Bitcode 序列化与反序列化全量效验 (SortedSetObject & CompactZSet)
#[test]
fn test_bitcode_serialization_roundtrip() -> Void {
  // 7.1 SortedSetObject Bitcode 往返
  let mut z = SortedSetObject::new();
  z.zadd(1.5, b"elem1", ZAddOpt::default())?;
  z.zadd(2.5, b"elem2", ZAddOpt::default())?;
  z.zadd(3.5, b"elem3", ZAddOpt::default())?;

  let future = Clock::now_since_epoch().as_millis() + 60_000;
  z.zexpire(b"elem2", future, ExpireOpt::default());

  let bytes = z.to_bitcode();
  let mut recovered = SortedSetObject::from_bitcode(&bytes)?;

  assert_eq!(recovered.len(), 3);
  assert_eq!(recovered.zscore(b"elem1"), Some(1.5));
  assert_eq!(recovered.zscore(b"elem2"), Some(2.5));
  assert_eq!(recovered.zscore(b"elem3"), Some(3.5));
  assert!(recovered.zttl(b"elem2") > 0);
  assert_eq!(recovered.zttl(b"elem1"), -1);

  // 7.2 CompactZSet Bitcode 往返
  let mut compact = CompactZSet::new();
  compact.zadd(10.0, b"c1", ZAddOpt::default())?;
  compact.zadd(20.0, b"c2", ZAddOpt::default())?;

  let c_bytes = compact.to_bitcode();
  let c_recovered = CompactZSet::from_bitcode(&c_bytes)?;
  assert_eq!(c_recovered.len(), 2);
  assert_eq!(c_recovered.zscore(b"c1"), Some(10.0));
  assert_eq!(c_recovered.zscore(b"c2"), Some(20.0));

  // 7.3 CompactZSet pop_min / pop_max
  let mut compact_pop = c_recovered;
  assert_eq!(compact_pop.pop_min(), Some((b"c1".to_vec(), 10.0)));
  assert_eq!(compact_pop.pop_max(), Some((b"c2".to_vec(), 20.0)));
  assert_eq!(compact_pop.len(), 0);

  OK
}
