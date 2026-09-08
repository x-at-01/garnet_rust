//! GEO 地理位置演示
//!
//! 底层原语（geohash 编解码 / 大圆距离 / 半径过滤）与
//! `SortedSetObject` 对象级 GEO API（GEOADD/GEODIST/GEOPOS/GEOHASH）。

use wedb_zset::{
  SortedSetObject,
  geo::{decode_geohash, encode_geohash, geo_distance, geo_filter_radius, get_geohash_code},
};

/// 演示城市 (名称, (纬度, 经度))
const CITIES: [(&str, (f64, f64)); 4] = [
  ("beijing", (39.9042, 116.4074)),
  ("shanghai", (31.2304, 121.4737)),
  ("hangzhou", (30.2741, 120.1551)),
  ("chengdu", (30.5728, 104.0668)),
];

fn main() {
  // ---- 底层原语 ----
  let (lat, lon) = CITIES[0].1;
  let code = encode_geohash(lat, lon).expect("合法经纬度");
  println!(
    "{} 52 位整数 geohash = {code}，base32 = {}",
    CITIES[0].0,
    get_geohash_code(code)
  );
  let (lat2, lon2) = decode_geohash(code);
  println!("解码还原 ≈ ({lat2:.4}, {lon2:.4})");

  let (sh_lat, sh_lon) = CITIES[1].1;
  println!(
    "北京 → 上海 ≈ {:.0} km",
    geo_distance(lat, lon, sh_lat, sh_lon) / 1000.0
  );

  // 批量半径过滤：返回 (城市下标, 距离米)，包围盒预筛 + 半正矢大圆距离精算
  let (lats, lons): (Vec<_>, Vec<_>) = CITIES.iter().map(|(_, (la, lo))| (*la, *lo)).unzip();
  let hits: Vec<(usize, String)> = geo_filter_radius(sh_lat, sh_lon, 1_200_000.0, &lats, &lons)
    .into_iter()
    .map(|(i, dist)| (i, format!("{}（{:.0} km）", CITIES[i].0, dist / 1000.0)))
    .collect();
  println!("距上海 1200 km 内 = {hits:?}");

  // ---- 对象级 GEO API ----
  let mut map = SortedSetObject::new();
  for (name, (la, lo)) in CITIES {
    map.geoadd(la, lo, name).expect("合法经纬度");
  }
  println!(
    "GEODIST beijing→hangzhou = {:.0} m",
    map
      .geodist(b"beijing", b"hangzhou")
      .expect("成员存在且有坐标")
  );
  let (pos_lat, pos_lon) = map.geopos(b"chengdu").expect("成员存在且有坐标");
  println!("GEOPOS chengdu = ({pos_lat:.4}, {pos_lon:.4})");
  println!("GEOHASH = {:?}", map.geohash(&[b"beijing", b"shanghai"]));
}
