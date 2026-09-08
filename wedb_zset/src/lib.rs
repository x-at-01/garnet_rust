#![cfg_attr(docsrs, feature(doc_cfg))]

mod compact;
mod error;
pub mod geo;
mod skiplist;
mod zset;

pub use compact::{
  CompactEntryRef, CompactZSet, CompactZSetCodec, CompactZSetExt, CompactZSetIter,
};
pub use error::{Error, Result};
pub use geo::{
  BITS_OF_PRECISION, CODE_LENGTH, EARTH_RADIUS_IN_METERS, GeoBoundingBox, GeoDistanceUnit, GeoItem,
  GeoOrder, GeoOrigin, GeoSearchOpt, GeoShape, LATITUDE_MAX, LATITUDE_MIN, LONGITUDE_MAX,
  LONGITUDE_MIN, METERS_PER_DEG_LAT, TWO_EARTH_RADIUS_IN_METERS, convert_meters_to_units,
  convert_value_to_meters, decode_geohash, encode_geohash, geo_distance, geo_distance_batch,
  geo_filter_radius, geohash_code_bytes, get_distance_when_in_rectangle,
  get_geo_error_by_precision, get_geohash_code, is_point_within_radius, simd_batch_distance,
  write_geohash_code,
};
pub use skiplist::{ScoreRange, SkipList};
pub use zset::{
  ExpireOpt, ExpireResult, FORMAT_VERSION, LexBound, SortableFloat, SortedSetAggregate,
  SortedSetEntryBitcode, SortedSetObject, ZAddOpt, decode_sortable_f64, encode_sortable_f64,
  format_f64, glob_match, write_f64,
};
