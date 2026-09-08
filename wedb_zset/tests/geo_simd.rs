use core::str::from_utf8;
use std::f64::consts::PI;

use aok::{OK, Void};
use log::info;
use wedb_zset::{
  BITS_OF_PRECISION, CODE_LENGTH, EARTH_RADIUS_IN_METERS, GeoDistanceUnit, LATITUDE_MAX,
  LATITUDE_MIN, LONGITUDE_MAX, LONGITUDE_MIN, TWO_EARTH_RADIUS_IN_METERS, convert_meters_to_units,
  convert_value_to_meters, decode_geohash, encode_geohash, geo, geo_distance, geo_distance_batch,
  geohash_code_bytes, get_distance_when_in_rectangle, get_geo_error_by_precision, get_geohash_code,
  is_point_within_radius, write_geohash_code,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 1. 极值与异常坐标测试（赤道、两极、日期变更线、本初子午线、NaN/无穷大与超界）
#[test]
fn test_extreme_and_invalid_coordinates() -> Void {
  // 经纬度常量范围对齐验证
  assert_eq!(LATITUDE_MIN, -90.0);
  assert_eq!(LATITUDE_MAX, 90.0);
  assert_eq!(LONGITUDE_MIN, -180.0);
  assert_eq!(LONGITUDE_MAX, 180.0);

  assert_eq!(LATITUDE_MIN, -90.0);
  assert_eq!(LATITUDE_MAX, 90.0);
  assert_eq!(LONGITUDE_MIN, -180.0);
  assert_eq!(LONGITUDE_MAX, 180.0);

  // 极端合法点
  let extreme_points = [
    (0.0, 0.0),      // 赤道与本初子午线交点
    (90.0, 0.0),     // 北极
    (-90.0, 0.0),    // 南极
    (0.0, 180.0),    // 赤道与日期变更线东
    (0.0, -180.0),   // 赤道与日期变更线西
    (90.0, 180.0),   // 东北极角
    (90.0, -180.0),  // 西北极角
    (-90.0, 180.0),  // 东南极角
    (-90.0, -180.0), // 西南极角
    (45.0, 90.0),    // 中纬度
    (-45.0, -90.0),  // 负中纬度
  ];

  let (lat_err, lon_err) = get_geo_error_by_precision();

  for (lat, lon) in extreme_points {
    let hash = encode_geohash(lat, lon)?;
    let (dec_lat, dec_lon) = decode_geohash(hash);

    // 解码后的点必须在理论网格中心误差范围内
    assert!(
      (dec_lat - lat).abs() <= lat_err + 1e-11,
      "纬度误差超限: lat={lat}, dec={dec_lat}, err={lat_err}"
    );
    assert!(
      (dec_lon - lon).abs() <= lon_err + 1e-11,
      "经度误差超限: lon={lon}, dec={dec_lon}, err={lon_err}"
    );

    // Base32 编码长度必须严格为 11，且末位为 '0'
    let code = get_geohash_code(hash);
    assert_eq!(code.len(), CODE_LENGTH);
    assert_eq!(code.as_bytes()[CODE_LENGTH - 1], b'0');
  }

  // 非法点测试（必须优雅返回错误，绝不 panic）
  let invalid_points = [
    (90.0000001, 0.0),
    (-90.0000001, 0.0),
    (0.0, 180.0000001),
    (0.0, -180.0000001),
    (100.0, 50.0),
    (-100.0, 50.0),
    (30.0, 200.0),
    (30.0, -200.0),
    (f64::NAN, 0.0),
    (0.0, f64::NAN),
    (f64::INFINITY, 0.0),
    (0.0, f64::NEG_INFINITY),
  ];

  for (lat, lon) in invalid_points {
    assert!(
      encode_geohash(lat, lon).is_err(),
      "非法坐标 ({lat}, {lon}) 应该报错"
    );
  }

  info!("test_extreme_and_invalid_coordinates passed");
  OK
}

/// 2. 随机 10000 点编解码往返无损与精度验证，并比对 SWAR 与硬件加速一致性
#[test]
fn test_roundtrip_precision_10000_points() -> Void {
  let mut rng = fastrand::Rng::with_seed(2026_0906);
  let (lat_err, lon_err) = get_geo_error_by_precision();

  // 编译期误差常量验证
  assert_eq!(lat_err, 180.0 / (1u64 << (BITS_OF_PRECISION / 2)) as f64);
  assert_eq!(lon_err, 360.0 / (1u64 << (BITS_OF_PRECISION / 2)) as f64);

  for _ in 0..10_000 {
    let lat = rng.f64() * 180.0 - 90.0;
    let lon = rng.f64() * 360.0 - 180.0;

    let hash = encode_geohash(lat, lon)?;
    let (dec_lat, dec_lon) = decode_geohash(hash);

    // 误差必须在 (lat_err, lon_err) 几何网格半宽内
    assert!((dec_lat - lat).abs() <= lat_err + 1e-12);
    assert!((dec_lon - lon).abs() <= lon_err + 1e-12);

    // 验证 SWAR 与当前选择的 Morton 编码完全一致
    let lat_quant = geo::quantize(lat, 1.0 / 180.0);
    let lon_quant = geo::quantize(lon, 1.0 / 360.0);

    let swar_hash = geo::morton_encode_swar(lat_quant, lon_quant);
    let fast_hash = geo::morton_encode(lat_quant, lon_quant);
    assert_eq!(swar_hash, fast_hash);

    let swar_dec = geo::morton_decode_swar(fast_hash);
    let fast_dec = geo::morton_decode(fast_hash);
    assert_eq!(swar_dec, fast_dec);
    assert_eq!(swar_dec, (lat_quant, lon_quant));

    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("bmi2") {
      unsafe {
        let bmi2_hash = geo::morton_encode_bmi2(lat_quant, lon_quant);
        assert_eq!(bmi2_hash, swar_hash);
        let bmi2_dec = geo::morton_decode_bmi2(fast_hash);
        assert_eq!(bmi2_dec, swar_dec);
      }
    }
  }

  info!("test_roundtrip_precision_10000_points passed");
  OK
}

/// 3. 批量 SIMD Haversine 距离计算与标量逐点对比（误差严格小于 1e-9，支持各类切片尺寸）
#[test]
fn test_simd_batch_distance_vs_scalar() -> Void {
  let mut rng = fastrand::Rng::with_seed(123456);

  let centers = [
    (39.9042, 116.4074), // 北京
    (31.2304, 121.4737), // 上海
    (0.0, 0.0),          // 本初子午线与赤道
    (90.0, 0.0),         // 北极
    (-90.0, 0.0),        // 南极
    (0.0, 180.0),        // 国际日期变更线
  ];

  // 测试不同长度的切片（0, 1, 2, 3, 4, 7, 8, 15, 16, 17, 100, 10000）
  let lengths = [0, 1, 2, 3, 4, 5, 7, 8, 15, 16, 17, 31, 32, 33, 100, 10_000];

  for (center_lat, center_lon) in centers {
    for &len in &lengths {
      let mut lats = Vec::with_capacity(len);
      let mut lons = Vec::with_capacity(len);
      for _ in 0..len {
        lats.push(rng.f64() * 180.0 - 90.0);
        lons.push(rng.f64() * 360.0 - 180.0);
      }

      let mut batch_dists = vec![0.0; len];
      geo_distance_batch(center_lat, center_lon, &lats, &lons, &mut batch_dists);

      for i in 0..len {
        let scalar_dist = geo_distance(center_lat, center_lon, lats[i], lons[i]);
        let diff = (batch_dists[i] - scalar_dist).abs();
        assert!(
          diff < 1e-9,
          "批量距离与标量不一致: batch={}, scalar={}, diff={diff}",
          batch_dists[i],
          scalar_dist
        );
      }
    }
  }

  // 两极与对跖点特殊大圆距离验证
  let north_pole = (90.0, 0.0);
  let south_pole = (-90.0, 0.0);
  let pole_distance = geo_distance(north_pole.0, north_pole.1, south_pole.0, south_pole.1);
  let expected_half_meridian = EARTH_RADIUS_IN_METERS * PI;
  assert!(
    (pole_distance - expected_half_meridian).abs() < 1e-3,
    "两极间大圆距离应为半经线长: pole_distance={pole_distance}, expected={expected_half_meridian}"
  );

  info!("test_simd_batch_distance_vs_scalar passed");
  OK
}

/// 4. 圆形与矩形区域范围点检索与距离判定测试
#[test]
fn test_circle_and_rectangle_queries() -> Void {
  let center_lat = 39.9042;
  let center_lon = 116.4074;

  // 1. 同一点距离为 0
  assert_eq!(
    is_point_within_radius(1.0, center_lat, center_lon, center_lat, center_lon),
    Some(0.0)
  );

  // 2. 距离判断 (上海距北京约 1060 ~ 1080 公里)
  let sh_lat = 31.2304;
  let sh_lon = 121.4737;
  let dist = geo_distance(center_lat, center_lon, sh_lat, sh_lon);

  assert!(is_point_within_radius(dist + 1.0, center_lat, center_lon, sh_lat, sh_lon).is_some());
  assert!(is_point_within_radius(dist - 1.0, center_lat, center_lon, sh_lat, sh_lon).is_none());
  assert!(is_point_within_radius(dist, center_lat, center_lon, sh_lat, sh_lon).is_some());

  // 3. 矩形判断 (axis-aligned rectangle) // 当目标点沿经度和纬度的距离分别小于等于 width/2 和 height/2 时在矩形内
  let width = 20_000.0; // 20 km
  let height = 20_000.0; // 20 km

  // 在矩形内的微小偏移点 (0.01 度约 1.1 km)
  let p_inside = (center_lat + 0.01, center_lon + 0.01);
  let res_in = get_distance_when_in_rectangle(
    width, height, center_lat, center_lon, p_inside.0, p_inside.1,
  );
  assert!(res_in.is_some());
  let actual_dist = geo_distance(center_lat, center_lon, p_inside.0, p_inside.1);
  assert!((res_in.unwrap() - actual_dist).abs() < 1e-9);

  // 超出矩形边界的点 (0.5 度远超 10 km)
  let p_outside = (center_lat + 0.5, center_lon);
  assert!(
    get_distance_when_in_rectangle(
      width,
      height,
      center_lat,
      center_lon,
      p_outside.0,
      p_outside.1
    )
    .is_none()
  );

  let p_outside_lon = (center_lat, center_lon + 0.5);
  assert!(
    get_distance_when_in_rectangle(
      width,
      height,
      center_lat,
      center_lon,
      p_outside_lon.0,
      p_outside_lon.1
    )
    .is_none()
  );

  info!("test_circle_and_rectangle_queries passed");
  OK
}

/// 5. Base32 编码与标准测试用例验证 (/ Redis 标准)
#[test]
fn test_base32_encoding() -> Void {
  // 知名基准测试城市坐标
  let cases = [
    // (lat, lon, expected_geohash_prefix) (39.9042, 116.4074, "wx4g0"), // 北京
    (31.2304, 121.4737, "wtw3s"), // 上海
    (48.8566, 2.3522, "u09tv"),   // 巴黎
    (40.7128, -74.0060, "dr5re"), // 纽约
    (0.0, 0.0, "s0000"),          // 零点
  ];

  for (lat, lon, prefix) in cases {
    let hash = encode_geohash(lat, lon)?;
    let code = get_geohash_code(hash);

    assert_eq!(code.len(), CODE_LENGTH);
    assert!(
      code.starts_with(prefix),
      "Geohash 前缀不符: code={code}, expected prefix={prefix}"
    );

    // 验证零分配写缓冲区版本与 String 版本一致
    let mut buf = [0u8; CODE_LENGTH];
    write_geohash_code(hash, &mut buf);
    assert_eq!(from_utf8(&buf).unwrap(), code);

    // 验证 geohash_code_bytes 零分配直接返回固定切片
    let stack_bytes = geohash_code_bytes(hash);
    assert_eq!(stack_bytes, buf);
  }

  assert_eq!(TWO_EARTH_RADIUS_IN_METERS, 2.0 * EARTH_RADIUS_IN_METERS);

  info!("test_base32_encoding passed");
  OK
}

/// 6. 单位转换测试 (GeoHash.ConvertValueToMeters / ConvertMetersToUnits)
#[test]
fn test_distance_unit_conversion() -> Void {
  let meters = 1000.0;

  // KM
  let km = convert_meters_to_units(meters, GeoDistanceUnit::KM);
  assert!((km - 1.0).abs() < 1e-12);
  let m_from_km = convert_value_to_meters(km, GeoDistanceUnit::KM);
  assert!((m_from_km - meters).abs() < 1e-12);

  // FT
  let ft = convert_meters_to_units(meters, GeoDistanceUnit::FT);
  assert!((ft - 3280.84).abs() < 1e-6);
  let m_from_ft = convert_value_to_meters(ft, GeoDistanceUnit::FT);
  assert!((m_from_ft - meters).abs() < 1e-6);

  // MI
  let mi = convert_meters_to_units(meters, GeoDistanceUnit::MI);
  assert!((mi - 0.621371).abs() < 1e-6);
  let m_from_mi = convert_value_to_meters(mi, GeoDistanceUnit::MI);
  assert!((m_from_mi - meters).abs() < 1e-6);

  // M
  let m = convert_meters_to_units(meters, GeoDistanceUnit::M);
  assert_eq!(m, meters);
  assert_eq!(convert_value_to_meters(meters, GeoDistanceUnit::M), meters);

  // 枚举关联方法
  assert_eq!(GeoDistanceUnit::KM.to_meters(2.5), 2500.0);
  assert_eq!(GeoDistanceUnit::KM.from_meters(2500.0), 2.5);

  // 字节解析
  assert_eq!(GeoDistanceUnit::from_bytes(b"m"), Some(GeoDistanceUnit::M));
  assert_eq!(
    GeoDistanceUnit::from_bytes(b"KM"),
    Some(GeoDistanceUnit::KM)
  );
  assert_eq!(
    GeoDistanceUnit::from_bytes(b"Ft"),
    Some(GeoDistanceUnit::FT)
  );
  assert_eq!(
    GeoDistanceUnit::from_bytes(b"mi"),
    Some(GeoDistanceUnit::MI)
  );
  assert_eq!(GeoDistanceUnit::from_bytes(b"invalid"), None);

  info!("test_distance_unit_conversion passed");
  OK
}
