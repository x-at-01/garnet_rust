use itoa::Buffer;

use crate::{
  config::{ClusterConfig, LOCAL_WORKER_ID},
  manager::RouteResult,
  slot::{SlotState, hash_slot},
};

/// 快速路由判定 (对标 Garnet ClusterSlotVerify)
#[inline]
pub fn route_request(
  config: &ClusterConfig,
  key: &[u8],
  is_asking: bool,
  key_exists_locally: bool,
) -> RouteResult {
  route_request_ext(config, key, is_asking, key_exists_locally, false)
}

#[inline]
fn make_endpoint_route<F>(config: &ClusterConfig, wid: usize, slot: u16, ctor: F) -> RouteResult
where
  F: FnOnce(u16, String) -> RouteResult,
{
  if wid == 0 {
    return RouteResult::ClusterDown;
  }
  let (ip, port) = config.get_worker_address(wid);
  if ip == "unassigned" || port == 0 {
    return RouteResult::ClusterDown;
  }
  let mut buf = Buffer::new();
  let mut endpoint = String::from(ip);
  endpoint.push(':');
  endpoint.push_str(buf.format(port));
  ctor(slot, endpoint)
}

/// 基于已计算槽位编号的扩展路由判定 (零二次哈希)
#[inline]
pub fn route_slot_ext(
  config: &ClusterConfig,
  slot: u16,
  is_asking: bool,
  key_exists_locally: bool,
  read_only: bool,
) -> RouteResult {
  let is_local = config.is_local(slot, read_only);
  let state = config.get_state(slot);

  if is_local {
    match state {
      SlotState::Stable => RouteResult::Ok(slot),
      SlotState::Migrating => {
        if key_exists_locally {
          RouteResult::Ok(slot)
        } else {
          let target_wid = config.slot_map[slot as usize].worker_id as usize;
          make_endpoint_route(config, target_wid, slot, |slot, endpoint| {
            RouteResult::Ask { slot, endpoint }
          })
        }
      }
      _ => RouteResult::ClusterDown,
    }
  } else {
    match state {
      SlotState::Importing if is_asking => RouteResult::Ok(slot),
      SlotState::Importing | SlotState::Stable => {
        let mut owner_wid = config.slot_map[slot as usize].effective_worker_id() as usize;
        if owner_wid == LOCAL_WORKER_ID
          && config.is_replica()
          && let Some(pri_id) = config.local_node_primary_id()
          && let p_wid = config.get_worker_id_from_node_id(pri_id)
          && p_wid > 0
        {
          owner_wid = p_wid;
        }
        make_endpoint_route(config, owner_wid, slot, |slot, endpoint| {
          RouteResult::Moved { slot, endpoint }
        })
      }
      _ => RouteResult::ClusterDown,
    }
  }
}

/// 支持只读模式的扩展路由判定
#[inline]
pub fn route_request_ext(
  config: &ClusterConfig,
  key: &[u8],
  is_asking: bool,
  key_exists_locally: bool,
  read_only: bool,
) -> RouteResult {
  let slot = hash_slot(key);
  route_slot_ext(config, slot, is_asking, key_exists_locally, read_only)
}
