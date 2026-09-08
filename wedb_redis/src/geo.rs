use wdev::Device;
use wedb_zset::ZAddOpt;
use wkv::StoreSession;

use super::{set::lock_keys_sorted, zset::zmadd_unlocked, *};
use crate::error::{Error, Result};

/// 地理搜索选项
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoSearchOpt<'a> {
  pub center: GeoSearchCenter<'a>,
  pub shape: GeoSearchShape,
  pub sort: GeoSortOrder,
  pub count: Option<usize>,
  pub count_any: bool,
}

/// 地理搜索中心点类型 (对标 Garnet GeoOriginType)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GeoSearchCenter<'a> {
  Coord { lon: f64, lat: f64 },
  Member(&'a [u8]),
}

/// 地理搜索几何区域类型 (对标 Garnet GeoSearchType)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GeoSearchShape {
  Radius {
    radius: f64,
    unit: wedb_zset::GeoDistanceUnit,
  },
  Box {
    width: f64,
    height: f64,
    unit: wedb_zset::GeoDistanceUnit,
  },
}

/// 地理搜索排序方向 (对标 Garnet GeoOrder)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GeoSortOrder {
  #[default]
  None,
  Asc,
  Desc,
}

/// 地理搜索命中项 (对标 Garnet GeoSearchData)
#[derive(Debug, Clone, PartialEq)]
pub struct GeoSearchResult {
  pub member: Vec<u8>,
  pub distance: f64,     // 距离中心点的距离 (米)
  pub score: f64,        // 52 位 Geohash 编码值
  pub coord: (f64, f64), // (lon, lat)
}

pub trait GeoCommands<D: Device> {
  /// 地理位置批量添加 (GEOADD)
  ///
  /// 先整体校验坐标并编码全部 geohash，再委托 zmadd 单次批量写入，
  /// 消除逐成员完整 meta/载荷读写放大（旧实现 O(N) 次独立 zadd 事务）
  async fn geoadd_multi(
    &self,
    key: &[u8],
    items: &[(f64, f64, &[u8])],
    nx: bool,
    xx: bool,
    ch: bool,
  ) -> Result<usize>;

  /// 地理位置添加 (GEOADD)
  async fn geoadd(&self, key: &[u8], lat: f64, lon: f64, member: impl AsRef<[u8]>) -> Result<bool>;

  /// 地理位置距离计算 (GEODIST)
  async fn geodist(&self, key: &[u8], member1: &[u8], member2: &[u8]) -> Result<Option<f64>>;

  /// 地理位置坐标查询 (GEOPOS)
  async fn geopos(&self, key: &[u8], member: &[u8]) -> Result<Option<(f64, f64)>>;

  /// 地理位置坐标批量查询 (GEOPOS)
  async fn geopos_multi(&self, key: &[u8], members: &[&[u8]]) -> Result<Vec<Option<(f64, f64)>>>;

  /// 地理位置 Base32 编码查询 (GEOHASH)
  async fn geohash(&self, key: &[u8], members: &[&[u8]]) -> Result<Vec<Option<String>>>;

  /// 地理区域检索 (GEOSEARCH / GEORADIUS / GEORADIUSBYMEMBER)
  async fn geosearch(&self, key: &[u8], opts: GeoSearchOpt<'_>) -> Result<Vec<GeoSearchResult>>;

  /// 地理区域检索并存储 (GEOSEARCHSTORE / GEORADIUS ... STORE)
  ///
  /// dest 与源键排序去重后整体获取独占桶锁（口径同 ZUNIONSTORE），
  /// 检索、dest 清空与结果写入全程原子；结果经 zmadd 无锁内核单批落盘，
  /// 消除旧实现逐成员 zadd 的重复加锁与逐条事务开销
  async fn geosearchstore(
    &self,
    dest_key: &[u8],
    src_key: &[u8],
    opts: GeoSearchOpt<'_>,
    store_dist: bool,
    unit: wedb_zset::GeoDistanceUnit,
  ) -> Result<usize>;
}

impl<D: Device> GeoCommands<D> for StoreSession<D> {
  /// 地理位置批量添加 (GEOADD)
  ///
  /// 先整体校验坐标并编码全部 geohash，再委托 zmadd 单次批量写入，
  /// 消除逐成员完整 meta/载荷读写放大（旧实现 O(N) 次独立 zadd 事务）
  async fn geoadd_multi(
    &self,
    key: &[u8],
    items: &[(f64, f64, &[u8])],
    nx: bool,
    xx: bool,
    ch: bool,
  ) -> Result<usize> {
    if nx && xx {
      return Err(wedb_zset::Error::InvalidOpt.into());
    }
    let opts = ZAddOpt {
      nx,
      xx,
      ch,
      ..Default::default()
    };
    // 坐标非法整体快速失败，不产生部分写入
    let mut encoded = Vec::with_capacity(items.len());
    for &(lon, lat, member) in items {
      if !(wedb_zset::LONGITUDE_MIN..=wedb_zset::LONGITUDE_MAX).contains(&lon)
        || !(wedb_zset::LATITUDE_MIN..=wedb_zset::LATITUDE_MAX).contains(&lat)
      {
        return Err(wedb_zset::Error::InvalidCoordinates.into());
      }
      let code = wedb_zset::encode_geohash(lat, lon)?;
      encoded.push((code as f64, member));
    }
    self.zmadd(key, encoded, opts).await
  }

  /// 地理位置添加 (GEOADD)
  async fn geoadd(&self, key: &[u8], lat: f64, lon: f64, member: impl AsRef<[u8]>) -> Result<bool> {
    let added = self
      .geoadd_multi(key, &[(lon, lat, member.as_ref())], false, false, false)
      .await?;
    Ok(added > 0)
  }

  /// 地理位置距离计算 (GEODIST)
  async fn geodist(&self, key: &[u8], member1: &[u8], member2: &[u8]) -> Result<Option<f64>> {
    let (s1, s2) = match (
      self.zscore(key, member1).await?,
      self.zscore(key, member2).await?,
    ) {
      (Some(s1), Some(s2)) => (s1, s2),
      _ => return Ok(None),
    };
    let (lat1, lon1) = wedb_zset::decode_geohash(s1 as u64);
    let (lat2, lon2) = wedb_zset::decode_geohash(s2 as u64);
    Ok(Some(wedb_zset::geo_distance(lat1, lon1, lat2, lon2)))
  }

  /// 地理位置坐标查询 (GEOPOS)
  async fn geopos(&self, key: &[u8], member: &[u8]) -> Result<Option<(f64, f64)>> {
    match self.zscore(key, member).await? {
      Some(score) => {
        let (lat, lon) = wedb_zset::decode_geohash(score as u64);
        Ok(Some((lon, lat)))
      }
      None => Ok(None),
    }
  }

  /// 地理位置坐标批量查询 (GEOPOS)
  async fn geopos_multi(&self, key: &[u8], members: &[&[u8]]) -> Result<Vec<Option<(f64, f64)>>> {
    let scores = self.zmscore(key, members).await?;
    let mut positions = Vec::with_capacity(scores.len());
    for opt in scores {
      positions.push(opt.map(|score| {
        let (lat, lon) = wedb_zset::decode_geohash(score as u64);
        (lon, lat)
      }));
    }
    Ok(positions)
  }

  /// 地理位置 Base32 编码查询 (GEOHASH)
  async fn geohash(&self, key: &[u8], members: &[&[u8]]) -> Result<Vec<Option<String>>> {
    let scores = self.zmscore(key, members).await?;
    let hashes = scores
      .into_iter()
      .map(|opt| opt.map(|score| wedb_zset::get_geohash_code(score as u64)))
      .collect();
    Ok(hashes)
  }

  /// 地理区域检索 (GEOSEARCH / GEORADIUS / GEORADIUSBYMEMBER)
  async fn geosearch(&self, key: &[u8], opts: GeoSearchOpt<'_>) -> Result<Vec<GeoSearchResult>> {
    let GeoSearchOpt {
      center,
      shape,
      sort,
      count,
      count_any,
    } = opts;
    let (center_lon, center_lat) = match center {
      GeoSearchCenter::Coord { lon, lat } => {
        if !(wedb_zset::LONGITUDE_MIN..=wedb_zset::LONGITUDE_MAX).contains(&lon)
          || !(wedb_zset::LATITUDE_MIN..=wedb_zset::LATITUDE_MAX).contains(&lat)
        {
          return Err(wedb_zset::Error::InvalidCoordinates.into());
        }
        (lon, lat)
      }
      GeoSearchCenter::Member(member) => match self.zscore(key, member).await? {
        Some(score) => {
          let (lat, lon) = wedb_zset::decode_geohash(score as u64);
          (lon, lat)
        }
        None => return Err(Error::ZSetMemberNotFound),
      },
    };

    let entries = self.zrange(key, 0, -1, false).await?;
    if entries.is_empty() {
      return Ok(Vec::new());
    }

    let mut results = Vec::new();

    match shape {
      GeoSearchShape::Radius { radius, unit } => {
        let radius_m = wedb_zset::convert_value_to_meters(radius, unit);
        for (member, score) in entries {
          let (lat, lon) = wedb_zset::decode_geohash(score as u64);
          if let Some(dist) =
            wedb_zset::is_point_within_radius(radius_m, center_lat, center_lon, lat, lon)
          {
            results.push(GeoSearchResult {
              member,
              distance: dist,
              score,
              coord: (lon, lat),
            });
            if count_any
              && let Some(cnt) = count
              && results.len() >= cnt
            {
              break;
            }
          }
        }
      }
      GeoSearchShape::Box {
        width,
        height,
        unit,
      } => {
        let width_m = wedb_zset::convert_value_to_meters(width, unit);
        let height_m = wedb_zset::convert_value_to_meters(height, unit);
        for (member, score) in entries {
          let (lat, lon) = wedb_zset::decode_geohash(score as u64);
          if let Some(dist) = wedb_zset::get_distance_when_in_rectangle(
            width_m, height_m, center_lat, center_lon, lat, lon,
          ) {
            results.push(GeoSearchResult {
              member,
              distance: dist,
              score,
              coord: (lon, lat),
            });
            if count_any
              && let Some(cnt) = count
              && results.len() >= cnt
            {
              break;
            }
          }
        }
      }
    }

    match sort {
      GeoSortOrder::Asc => results.sort_by(|a, b| a.distance.total_cmp(&b.distance)),
      GeoSortOrder::Desc => results.sort_by(|a, b| b.distance.total_cmp(&a.distance)),
      GeoSortOrder::None => {
        if !count_any && count.is_some() {
          results.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        }
      }
    }

    if let Some(cnt) = count
      && cnt < results.len()
    {
      results.truncate(cnt);
    }

    Ok(results)
  }

  /// 地理区域检索并存储 (GEOSEARCHSTORE / GEORADIUS ... STORE)
  ///
  /// dest 与源键排序去重后整体获取独占桶锁（口径同 ZUNIONSTORE），
  /// 检索、dest 清空与结果写入全程原子；结果经 zmadd 无锁内核单批落盘，
  /// 消除旧实现逐成员 zadd 的重复加锁与逐条事务开销
  async fn geosearchstore(
    &self,
    dest_key: &[u8],
    src_key: &[u8],
    opts: GeoSearchOpt<'_>,
    store_dist: bool,
    unit: wedb_zset::GeoDistanceUnit,
  ) -> Result<usize> {
    let _key_lock = lock_keys_sorted(self, dest_key, &[src_key])?;

    let results = self.geosearch(src_key, opts).await?;
    self.delete(dest_key).await?;
    if results.is_empty() {
      return Ok(0);
    }
    // 已持 dest 独占锁，直接复用无锁内核避免桶锁重入自锁
    let count = zmadd_unlocked(
      self,
      dest_key,
      results
        .iter()
        .map(|item| {
          // STOREDIST 时写入目标距离（按 unit 换算），否则回写原始 geohash 分值
          let score = if store_dist {
            wedb_zset::convert_meters_to_units(item.distance, unit)
          } else {
            item.score
          };
          (score, &item.member)
        })
        .collect::<Vec<_>>(),
      ZAddOpt::default(),
    )
    .await?;
    Ok(count)
  }
}
