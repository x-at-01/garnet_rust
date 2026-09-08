use std::f64::consts::PI;

use fearless_simd::{Level, Simd, dispatch};

use crate::error::{Error, Result};

/// WGS-84 / Pseudo-Mercator 最小经度
pub const LONGITUDE_MIN: f64 = -180.0;
/// WGS-84 / Pseudo-Mercator 最大经度
pub const LONGITUDE_MAX: f64 = 180.0;
/// WGS-84 / Pseudo-Mercator 最小纬度
pub const LATITUDE_MIN: f64 = -90.0;
/// WGS-84 / Pseudo-Mercator 最大纬度
pub const LATITUDE_MAX: f64 = 90.0;

/// 52 位 Geohash 精度位数 (对标 Garnet GeoHash.BitsOfPrecision)
pub const BITS_OF_PRECISION: usize = 52;
/// Geohash 标准 Base-32 文本长度 (对标 Garnet GeoHash.CodeLength)
pub const CODE_LENGTH: usize = 11;

/// 地球平均半径 (米，WGS-84 标准，对标 Garnet GeoHash.EarthRadiusInMeters)
pub const EARTH_RADIUS_IN_METERS: f64 = 6372797.560856;
/// 2 倍地球平均半径常量 (折叠半正矢公式乘 2 计算)
pub const TWO_EARTH_RADIUS_IN_METERS: f64 = 2.0 * EARTH_RADIUS_IN_METERS;

/// 角度转弧度系数
const DEG_TO_RAD: f64 = PI / 180.0;
/// 半角度转弧度系数 (常数折叠，避免计算 sin(d * 0.5) 时的二次乘法)
const HALF_DEG_TO_RAD: f64 = PI / 360.0;

/// 纬度每度对应地表距离 (米)
pub const METERS_PER_DEG_LAT: f64 = EARTH_RADIUS_IN_METERS * DEG_TO_RAD;

/// 地理坐标外接包围盒 (外接矩形初筛)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoBoundingBox {
  pub min_lat: f64,
  pub max_lat: f64,
  pub min_lon: f64,
  pub max_lon: f64,
}

impl GeoBoundingBox {
  /// 根据中心点经纬度与半径 (米) 构造外接包围盒
  #[inline]
  pub fn from_radius(center_lat: f64, center_lon: f64, radius_m: f64) -> Self {
    let dlat = radius_m / METERS_PER_DEG_LAT;
    let min_lat = (center_lat - dlat).max(LATITUDE_MIN);
    let max_lat = (center_lat + dlat).min(LATITUDE_MAX);

    let cos_lat = (center_lat * DEG_TO_RAD).abs().cos();
    let dlon = if cos_lat > 1e-6 {
      radius_m / (METERS_PER_DEG_LAT * cos_lat)
    } else {
      360.0
    };
    let min_lon = (center_lon - dlon).max(LONGITUDE_MIN);
    let max_lon = (center_lon + dlon).min(LONGITUDE_MAX);

    Self {
      min_lat,
      max_lat,
      min_lon,
      max_lon,
    }
  }

  /// 根据中心点经纬度与矩形宽高 (米) 构造外接包围盒
  #[inline]
  pub fn from_box(center_lat: f64, center_lon: f64, width_m: f64, height_m: f64) -> Self {
    let half_w = width_m * 0.5;
    let half_h = height_m * 0.5;
    let dlat = half_h / METERS_PER_DEG_LAT;
    let min_lat = (center_lat - dlat).max(LATITUDE_MIN);
    let max_lat = (center_lat + dlat).min(LATITUDE_MAX);

    let cos_lat = (center_lat * DEG_TO_RAD).abs().cos();
    let dlon = if cos_lat > 1e-6 {
      half_w / (METERS_PER_DEG_LAT * cos_lat)
    } else {
      360.0
    };
    let min_lon = (center_lon - dlon).max(LONGITUDE_MIN);
    let max_lon = (center_lon + dlon).min(LONGITUDE_MAX);

    Self {
      min_lat,
      max_lat,
      min_lon,
      max_lon,
    }
  }

  /// 检查点是否在外接矩形内
  #[inline(always)]
  pub fn contains(&self, lat: f64, lon: f64) -> bool {
    lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
  }
}

/// Base-32 编码字符集
const BASE32_CHARS: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// 地理距离单位枚举 (对标 Garnet GeoDistanceUnitType)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum GeoDistanceUnit {
  #[default]
  M = 0,
  KM = 1,
  FT = 2,
  MI = 3,
}

impl GeoDistanceUnit {
  /// 从字节切片解析单位 (不区分大小写，如 b"m", b"km", b"ft", b"mi")
  #[inline]
  pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
    if bytes.eq_ignore_ascii_case(b"m") {
      Some(Self::M)
    } else if bytes.eq_ignore_ascii_case(b"km") {
      Some(Self::KM)
    } else if bytes.eq_ignore_ascii_case(b"ft") {
      Some(Self::FT)
    } else if bytes.eq_ignore_ascii_case(b"mi") {
      Some(Self::MI)
    } else {
      None
    }
  }

  /// 转换为米
  #[inline]
  pub fn to_meters(self, value: f64) -> f64 {
    convert_value_to_meters(value, self)
  }

  /// 从米转换为当前单位
  #[inline]
  pub fn from_meters(self, value: f64) -> f64 {
    convert_meters_to_units(value, self)
  }
}

/// 将指定单位的数值转换为米 (对标 Garnet GeoHash.ConvertValueToMeters)
#[inline]
pub fn convert_value_to_meters(value: f64, unit: GeoDistanceUnit) -> f64 {
  match unit {
    GeoDistanceUnit::M => value,
    GeoDistanceUnit::KM => value / 0.001,
    GeoDistanceUnit::FT => value / 3.28084,
    GeoDistanceUnit::MI => value / 0.000621371,
  }
}

/// 将以米为单位的数值转换为指定单位 (对标 Garnet GeoHash.ConvertMetersToUnits)
#[inline]
pub fn convert_meters_to_units(value: f64, unit: GeoDistanceUnit) -> f64 {
  match unit {
    GeoDistanceUnit::M => value,
    GeoDistanceUnit::KM => value * 0.001,
    GeoDistanceUnit::FT => value * 3.28084,
    GeoDistanceUnit::MI => value * 0.000621371,
  }
}

/// 计算当前精度下的经纬度最大误差 (对标 Garnet GeoHash.GetGeoErrorByPrecision)
#[inline]
pub const fn get_geo_error_by_precision() -> (f64, f64) {
  const LAT_BITS: u32 = (BITS_OF_PRECISION / 2) as u32;
  const LON_BITS: u32 = (BITS_OF_PRECISION - BITS_OF_PRECISION / 2) as u32;
  const LAT_ERROR: f64 = 180.0 / (1u64 << LAT_BITS) as f64;
  const LON_ERROR: f64 = 360.0 / (1u64 << LON_BITS) as f64;
  (LAT_ERROR, LON_ERROR)
}

/// 将浮点坐标量化为 32 位无符号整数
///
/// 利用 IEEE-754 浮点数表示快速提取高位有效数字
#[inline(always)]
pub fn quantize(value: f64, range_reciprocal: f64) -> u32 {
  let y = (value.mul_add(range_reciprocal, 1.5).to_bits()) >> 20;
  const MAX_Y: u64 = 2.0f64.to_bits() >> 20;
  if y == MAX_Y { u32::MAX } else { y as u32 }
}

/// 将 32 位量化整数还原为浮点坐标下界
#[inline(always)]
pub fn dequantize(quantized_value: u32, range_max: f64) -> f64 {
  let value = f64::from_bits(((quantized_value as u64) << 20) | (1023u64 << 52));
  (range_max + range_max).mul_add(value - 1.0, -range_max)
}

/// SWAR 算法：将 32 位整数的各个位按偶数位展开（Magic Bits 展开）
#[inline(always)]
pub fn spread(x: u32) -> u64 {
  let mut y = x as u64;
  y = (y | (y << 16)) & 0x0000_FFFF_0000_FFFF;
  y = (y | (y << 8)) & 0x00FF_00FF_00FF_00FF;
  y = (y | (y << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
  y = (y | (y << 2)) & 0x3333_3333_3333_3333;
  y = (y | (y << 1)) & 0x5555_5555_5555_5555;
  y
}

/// SWAR 算法：将 64 位整数中偶数位提取并压紧为 32 位整数（Magic Bits 压紧）
#[inline(always)]
pub fn squash(mut x: u64) -> u32 {
  x &= 0x5555_5555_5555_5555;
  x = (x | (x >> 1)) & 0x3333_3333_3333_3333;
  x = (x | (x >> 2)) & 0x0F0F_0F0F_0F0F_0F0F;
  x = (x | (x >> 4)) & 0x00FF_00FF_00FF_00FF;
  x = (x | (x >> 8)) & 0x0000_FFFF_0000_FFFF;
  x = (x | (x >> 16)) & 0x0000_0000_FFFF_FFFF;
  x as u32
}

/// 基于 SWAR 的 Morton 编码
#[inline(always)]
pub fn morton_encode_swar(x: u32, y: u32) -> u64 {
  spread(x) | (spread(y) << 1)
}

/// 基于 SWAR 的 Morton 解码
#[inline(always)]
pub fn morton_decode_swar(x: u64) -> (u32, u32) {
  (squash(x), squash(x >> 1))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
#[inline]
pub unsafe fn morton_encode_bmi2(x: u32, y: u32) -> u64 {
  use core::arch::x86_64::_pdep_u64;
  _pdep_u64(x as u64, 0x5555_5555_5555_5555) | _pdep_u64(y as u64, 0xAAAA_AAAA_AAAA_AAAA)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
#[inline]
pub unsafe fn morton_decode_bmi2(x: u64) -> (u32, u32) {
  use core::arch::x86_64::_pext_u64;
  (
    _pext_u64(x, 0x5555_5555_5555_5555) as u32,
    _pext_u64(x, 0xAAAA_AAAA_AAAA_AAAA) as u32,
  )
}

/// 极致硬件加速 Morton 编码（Z-Order Curve）
///
/// x 坐标置于偶数位，y 坐标置于奇数位
#[inline(always)]
pub fn morton_encode(x: u32, y: u32) -> u64 {
  #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
  {
    // SAFETY: bmi2 经 rustc 编译期确定开启
    return unsafe { morton_encode_bmi2(x, y) };
  }
  #[cfg(all(target_arch = "x86_64", not(target_feature = "bmi2")))]
  {
    if is_x86_feature_detected!("bmi2") {
      // SAFETY: bmi2 经动态 CPUID 检测确认支持
      return unsafe { morton_encode_bmi2(x, y) };
    }
  }
  morton_encode_swar(x, y)
}

/// 极致硬件加速 Morton 解码（Z-Order Curve）
///
/// 提取偶数位还原为 x 坐标，提取奇数位还原为 y 坐标
#[inline(always)]
pub fn morton_decode(x: u64) -> (u32, u32) {
  #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
  {
    // SAFETY: bmi2 经 rustc 编译期确定开启
    return unsafe { morton_decode_bmi2(x) };
  }
  #[cfg(all(target_arch = "x86_64", not(target_feature = "bmi2")))]
  {
    if is_x86_feature_detected!("bmi2") {
      // SAFETY: bmi2 经动态 CPUID 检测确认支持
      return unsafe { morton_decode_bmi2(x) };
    }
  }
  morton_decode_swar(x)
}

/// 将经纬度编码为 52 位整数 geohash (对标 Garnet GeoHash.GeoToLongValue)
#[inline]
pub fn encode_geohash(lat: f64, lon: f64) -> Result<u64> {
  if !(LATITUDE_MIN..=LATITUDE_MAX).contains(&lat)
    || !(LONGITUDE_MIN..=LONGITUDE_MAX).contains(&lon)
  {
    return Err(Error::InvalidCoordinates);
  }

  const LAT_TO_UNIT_RANGE_RECIPROCAL: f64 = 1.0 / 180.0;
  const LON_TO_UNIT_RANGE_RECIPROCAL: f64 = 1.0 / 360.0;

  let lat_quantized = quantize(lat, LAT_TO_UNIT_RANGE_RECIPROCAL);
  let lon_quantized = quantize(lon, LON_TO_UNIT_RANGE_RECIPROCAL);

  let result = morton_encode(lat_quantized, lon_quantized);
  Ok(result >> (64 - BITS_OF_PRECISION))
}

/// 将 52 位整数 geohash 解码为经纬度 (lat, lon) (对标 Garnet GeoHash.GetCoordinatesFromLong)
#[inline]
pub fn decode_geohash(hash: u64) -> (f64, f64) {
  let full_hash = hash << (64 - BITS_OF_PRECISION);
  let (lat_quantized, lon_quantized) = morton_decode(full_hash);

  let min_lat = dequantize(lat_quantized, LATITUDE_MAX);
  let min_lon = dequantize(lon_quantized, LONGITUDE_MAX);

  let (lat_error, lon_error) = get_geo_error_by_precision();

  let lat = min_lat + (lat_error * 0.5);
  let lon = min_lon + (lon_error * 0.5);
  (lat, lon)
}

/// 写入 11 位 Base-32 编码字节到指定缓冲区 (零堆内存分配版本)
#[inline]
pub fn write_geohash_code(mut hash: u64, out: &mut [u8; CODE_LENGTH]) {
  out[CODE_LENGTH - 1] = b'0';
  for b in &mut out[..CODE_LENGTH - 1] {
    let idx = ((hash >> (BITS_OF_PRECISION - 5)) & 0x1F) as usize;
    // SAFETY: idx 由 & 0x1F 掩码严格约束在 [0, 31]，BASE32_CHARS 长度为 32，绝对不会越界
    *b = unsafe { *BASE32_CHARS.get_unchecked(idx) };
    hash <<= 5;
  }
}

/// 生成 11 位 Base-32 编码字节数组 (100% 栈分配，零堆内存分配)
#[inline(always)]
pub fn geohash_code_bytes(hash: u64) -> [u8; CODE_LENGTH] {
  let mut buf = [b'0'; CODE_LENGTH];
  write_geohash_code(hash, &mut buf);
  buf
}

/// 将 52 位 geohash 整数转为标准的 11 位 Base-32 字符串 (对标 Garnet GeoHash.GetGeoHashCode)
pub fn get_geohash_code(hash: u64) -> String {
  let buf = geohash_code_bytes(hash);
  // SAFETY: 写入的字符全部来自 ASCII Base-32 字符集
  unsafe { String::from_utf8_unchecked(buf.to_vec()) }
}

/// 计算两经纬度之间的半正矢大圆距离 (米) (对标 Garnet GeoHash.Distance)
#[inline]
pub fn geo_distance(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
  if lat1 == lat2 && lon1 == lon2 {
    return 0.0;
  }
  let rad_lat1 = lat1 * DEG_TO_RAD;
  let rad_lat2 = lat2 * DEG_TO_RAD;
  let half_dlat = (lat2 - lat1) * HALF_DEG_TO_RAD;
  let half_dlon = (lon2 - lon1) * HALF_DEG_TO_RAD;

  let sin_half_dlat = half_dlat.sin();
  let sin_half_dlon = half_dlon.sin();
  let lat_haversine = sin_half_dlat * sin_half_dlat;
  let lon_haversine = sin_half_dlon * sin_half_dlon;

  let tmp = rad_lat1.cos() * rad_lat2.cos();
  let a = tmp.mul_add(lon_haversine, lat_haversine).clamp(0.0, 1.0);
  let c = a.sqrt().asin();

  TWO_EARTH_RADIUS_IN_METERS * c
}

/// 判断目标坐标是否在以中心点为圆心、指定半径 (米) 的圆形区域内 (对标 Garnet GeoHash.IsPointWithinRadius)
///
/// 若在区域内则返回 `Some(distance)`，否则返回 `None`
#[inline]
pub fn is_point_within_radius(
  radius: f64,
  lat_center: f64,
  lon_center: f64,
  lat: f64,
  lon: f64,
) -> Option<f64> {
  if radius < 0.0 {
    return None;
  }
  if lat == lat_center && lon == lon_center {
    return if radius >= 0.0 { Some(0.0) } else { None };
  }
  // 快速外接包围盒初筛：纬度差对应的大圆距离超过半径，必不在区域内
  let dlat = (lat - lat_center).abs();
  if dlat * METERS_PER_DEG_LAT > radius {
    return None;
  }
  let distance = geo_distance(lat_center, lon_center, lat, lon);
  if distance <= radius {
    Some(distance)
  } else {
    None
  }
}

/// 判断目标坐标是否在以中心点为中心、指定长宽 (米) 的轴对齐矩形区域内 (对标 Garnet GeoHash.GetDistanceWhenInRectangle)
///
/// 若在矩形内则返回中心点到目标点的距离 `Some(distance)`，否则返回 `None`
#[inline]
pub fn get_distance_when_in_rectangle(
  width_m: f64,
  height_m: f64,
  lat_center: f64,
  lon_center: f64,
  lat2: f64,
  lon2: f64,
) -> Option<f64> {
  let half_height = height_m * 0.5;
  // 经线方向纬度大圆距离快速判定：若纬度跨度超标立即剪枝返回 None
  let lat_distance = (lat2 - lat_center).abs() * METERS_PER_DEG_LAT;
  if lat_distance > half_height {
    return None;
  }
  let half_width = width_m * 0.5;
  let lon_distance = geo_distance(lat2, lon2, lat2, lon_center);
  if lon_distance > half_width {
    return None;
  }
  Some(geo_distance(lat_center, lon_center, lat2, lon2))
}

/// SIMD 多版本自动向量化 Haversine 距离批处理内核
#[inline(always)]
fn haversine_batch_kernel<S: Simd>(
  _simd: S,
  center_lat: f64,
  center_lon: f64,
  lats: &[f64],
  lons: &[f64],
  distances: &mut [f64],
) {
  let rad_center_lat = center_lat * DEG_TO_RAD;
  let cos_center_lat = rad_center_lat.cos();

  for ((&lat2, &lon2), dist) in lats.iter().zip(lons.iter()).zip(distances.iter_mut()) {
    let rad_lat2 = lat2 * DEG_TO_RAD;
    let half_dlat = (lat2 - center_lat) * HALF_DEG_TO_RAD;
    let half_dlon = (lon2 - center_lon) * HALF_DEG_TO_RAD;

    let sin_half_dlat = half_dlat.sin();
    let sin_half_dlon = half_dlon.sin();
    let lat_haversine = sin_half_dlat * sin_half_dlat;
    let lon_haversine = sin_half_dlon * sin_half_dlon;

    let tmp = cos_center_lat * rad_lat2.cos();
    let a = tmp.mul_add(lon_haversine, lat_haversine).clamp(0.0, 1.0);
    let c = a.sqrt().asin();

    *dist = TWO_EARTH_RADIUS_IN_METERS * c;
  }
}

/// 批量并行计算多个坐标点到中心点的 Haversine 距离 (米)
///
/// 利用 fearless_simd 的动态分发机制，在不同 CPU 架构上展开 SIMD 自动向量化
pub fn geo_distance_batch(
  center_lat: f64,
  center_lon: f64,
  lats: &[f64],
  lons: &[f64],
  distances: &mut [f64],
) {
  let count = lats.len().min(lons.len()).min(distances.len());
  if count == 0 {
    return;
  }
  let lats = &lats[..count];
  let lons = &lons[..count];
  let distances = &mut distances[..count];

  let level = Level::new();
  dispatch!(
    level,
    simd => haversine_batch_kernel(simd, center_lat, center_lon, lats, lons, distances)
  );
}

/// 批量并行计算多个坐标点到中心点的 Haversine 距离 (米) (对标 simd_batch_distance)
pub use geo_distance_batch as simd_batch_distance;

/// 批量过滤落在以指定中心点为圆心、指定半径 (米) 内的坐标
///
/// 采用两级加速流水线：
/// 1. 外接包围盒快速初筛 (零三角函数运算，快速剔除远距离点)
/// 2. 半正矢大圆距离精确判定
///
/// 边界语义：距离恰等于半径的点计入 (含边界，对标 Redis GEORADIUS)
/// 返回满足条件的 `Vec<(点索引, 距离)>`
pub fn geo_filter_radius(
  center_lat: f64,
  center_lon: f64,
  radius_m: f64,
  lats: &[f64],
  lons: &[f64],
) -> Vec<(usize, f64)> {
  if radius_m < 0.0 {
    return Vec::new();
  }
  let count = lats.len().min(lons.len());
  if count == 0 {
    return Vec::new();
  }
  let bbox = GeoBoundingBox::from_radius(center_lat, center_lon, radius_m);

  lats[..count]
    .iter()
    .zip(&lons[..count])
    .enumerate()
    .filter_map(|(i, (&lat, &lon))| {
      bbox
        .contains(lat, lon)
        .then(|| geo_distance(center_lat, center_lon, lat, lon))
        .filter(|&dist| dist <= radius_m)
        .map(|dist| (i, dist))
    })
    .collect()
}

/// GEO 查询原点 (对标 Garnet GeoOriginType)
#[derive(Debug, Clone, PartialEq)]
pub enum GeoOrigin {
  Coord { lon: f64, lat: f64 },
  Member(Vec<u8>),
}

/// GEO 搜索形状区域 (对标 Garnet GeoSearchType)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GeoShape {
  ByRadius {
    radius: f64,
    unit: GeoDistanceUnit,
  },
  ByBox {
    width: f64,
    height: f64,
    unit: GeoDistanceUnit,
  },
}

/// 排序方向 (对标 Garnet GeoOrder)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GeoOrder {
  #[default]
  None,
  Asc,
  Desc,
}

/// GEOSEARCH 查询选项 (对标 Garnet GeoSearchOpt)
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GeoSearchOpt {
  pub order: GeoOrder,
  pub count: Option<usize>,
  pub any: bool,
  pub with_dist: bool,
  pub with_coord: bool,
  pub with_hash: bool,
}

/// 单个 GEO 匹配结果条目 (对标 Garnet GeoSearchData)
#[derive(Debug, Clone, PartialEq)]
pub struct GeoItem {
  pub member: Vec<u8>,
  pub dist: Option<f64>,
  pub hash: Option<u64>,
  pub coord: Option<(f64, f64)>, // (lon, lat)
}
