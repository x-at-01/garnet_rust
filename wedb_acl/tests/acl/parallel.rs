//! ParallelTests：并发场景下的密码哈希、认证与 SETUSER 测试

use std::{sync::Arc, thread};

use aok::{OK, Void};
use log::info;
use wedb_acl::{AccessControlList, AclPassword};

use crate::support::{DUMMY_PASSWORD, DUMMY_PASSWORD_B, TEST_USER_A};

/// 对标 Garnet ParallelTests.cs: ParallelPasswordHashTest —— 多线程并发密码哈希无竞态
#[test]
fn test_parallel_password_hash() -> Void {
  let handles: Vec<_> = (0..8)
    .map(|_| {
      thread::spawn(|| {
        for _ in 0..500 {
          let _ = AclPassword::from_cleartext(DUMMY_PASSWORD);
          let _ = AclPassword::from_cleartext(DUMMY_PASSWORD_B);
        }
      })
    })
    .collect();
  for h in handles {
    h.join().unwrap();
  }
  info!("C# 兼容性测试：ParallelPasswordHashTest 通过");
  OK
}

/// 对标 Garnet ParallelTests.cs: ParallelAuthTest —— 多线程并发认证结果稳定
#[test]
fn test_parallel_auth() -> Void {
  let acl = Arc::new(AccessControlList::new(""));
  acl.set_user(TEST_USER_A, &["on", &format!(">{}", DUMMY_PASSWORD)])?;

  let handles: Vec<_> = (0..8)
    .map(|_| {
      let acl = acl.clone();
      thread::spawn(move || {
        for _ in 0..500 {
          assert!(acl.auth(TEST_USER_A, DUMMY_PASSWORD));
          assert!(!acl.auth(TEST_USER_A, DUMMY_PASSWORD_B));
        }
      })
    })
    .collect();
  for h in handles {
    h.join().unwrap();
  }
  info!("C# 兼容性测试：ParallelAuthTest 通过");
  OK
}

/// 对标 Garnet ParallelTests.cs: ParallelAclSetUserTest —— 多线程并发修改各自用户无竞争错误
#[test]
fn test_parallel_acl_setuser() -> Void {
  let acl = Arc::new(AccessControlList::new(""));
  let handles: Vec<_> = (0..8)
    .map(|i| {
      let acl = acl.clone();
      thread::spawn(move || {
        let uname = format!("user_{}", i);
        for _ in 0..200 {
          acl.set_user(&uname, &["on", "+get"]).unwrap();
          acl.set_user(&uname, &["off", "-get"]).unwrap();
        }
      })
    })
    .collect();
  for h in handles {
    h.join().unwrap();
  }
  info!("C# 兼容性测试：ParallelAclSetUserTest 通过");
  OK
}

/// 对标 Garnet ParallelTests.cs: ParallelAclSetUserAvoidsMapContentionTest —— 多线程并发创建用户避免哈希表竞争
#[test]
fn test_parallel_acl_setuser_avoids_map_contention() -> Void {
  let acl = Arc::new(AccessControlList::new(""));
  let handles: Vec<_> = (0..8)
    .map(|i| {
      let acl = acl.clone();
      thread::spawn(move || {
        for j in 0..100 {
          let uname = format!("user_{}_{}", i, j);
          acl.set_user(&uname, &["on", "+@all"]).unwrap();
          assert!(acl.get_user(&uname).is_some());
        }
      })
    })
    .collect();
  for h in handles {
    h.join().unwrap();
  }
  info!("C# 兼容性测试：ParallelAclSetUserAvoidsMapContentionTest 通过");
  OK
}
