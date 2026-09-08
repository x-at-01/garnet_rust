use std::{
  str::from_utf8,
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicI64, Ordering},
  },
  thread,
};

use aok::{OK, Void};
use log::info;
use wedb_module::{
  CustomCommandInfo, CustomProcedure, Error, Module, ModuleActionStatus, ModuleApi, ModuleRegistry,
  RespValue, parse_module_spec,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 测试用空实现 API：一律回 OK
struct NoopApi;

impl ModuleApi for NoopApi {
  fn call(&self, _cmd: &str, _args: &[&[u8]]) -> wedb_module::Result<RespValue> {
    Ok(RespValue::ok())
  }
}

/// 计数器演示模块：初始化并注册 MOD.COUNTER 过程
struct CounterModule;

impl Module for CounterModule {
  fn on_load(&self, ctx: &mut wedb_module::ModuleLoadContext<'_>, _args: &[&[u8]]) {
    if ctx.initialize("counter", 1) != ModuleActionStatus::Success {
      // 重复加载等场景由注册表返回错误，这里直接结束注册
      return;
    }
    let hits = Arc::new(AtomicI64::new(0));
    let status = ctx.register_procedure(
      "MOD.COUNTER",
      move |_: &dyn ModuleApi, args: &[&[u8]], out: &mut Vec<u8>| {
        let delta: i64 = args
          .first()
          .and_then(|a| from_utf8(a).ok())
          .and_then(|s| s.parse().ok())
          .unwrap_or(1);
        let v = hits.fetch_add(delta, Ordering::Relaxed) + delta;
        RespValue::Integer(v).write_resp(out);
        Ok(())
      },
      Some(CustomCommandInfo::arity(-1)),
    );
    assert_eq!(status, ModuleActionStatus::Success);
    info!("counter 模块已加载");
  }
}

/// 不调用 initialize 的坏模块
struct BareModule;

impl Module for BareModule {
  fn on_load(&self, _ctx: &mut wedb_module::ModuleLoadContext<'_>, _args: &[&[u8]]) {}
}

/// 空名坏模块
struct EmptyNameModule;

impl Module for EmptyNameModule {
  fn on_load(&self, ctx: &mut wedb_module::ModuleLoadContext<'_>, _args: &[&[u8]]) {
    assert_eq!(
      ctx.initialize("", 1),
      ModuleActionStatus::InvalidRegistrationInfo
    );
  }
}

/// 校验注册前置条件的模块：未 initialize 即注册、空命令名、同名命令重注册
struct GuardModule;

impl Module for GuardModule {
  fn on_load(&self, ctx: &mut wedb_module::ModuleLoadContext<'_>, _args: &[&[u8]]) {
    // 未 initialize 即注册被拒绝
    assert_eq!(
      ctx.register_procedure("G.NOINIT", EchoProc, None),
      ModuleActionStatus::Failure
    );
    assert_eq!(ctx.initialize("guard", 1), ModuleActionStatus::Success);
    // 重复 initialize 被拒绝且不登记新名
    assert_eq!(
      ctx.initialize("guard2", 2),
      ModuleActionStatus::AlreadyLoaded
    );
    // 空命令名被拒绝
    assert_eq!(
      ctx.register_procedure("", EchoProc, None),
      ModuleActionStatus::InvalidRegistrationInfo
    );
    assert_eq!(
      ctx.register_procedure("G.CMD", EchoProc, None),
      ModuleActionStatus::Success
    );
    // 同名命令重注册被拒绝
    assert_eq!(
      ctx.register_procedure("G.CMD", EchoProc, None),
      ModuleActionStatus::AlreadyExists
    );
  }
}

/// 捕获加载参数的模块：MOD.ARG 回显 (执行参数 + 加载参数)
struct ArgsModule;

impl Module for ArgsModule {
  fn on_load(&self, ctx: &mut wedb_module::ModuleLoadContext<'_>, args: &[&[u8]]) {
    assert_eq!(ctx.initialize("argsmod", 1), ModuleActionStatus::Success);
    let prefix = args.first().copied().unwrap_or_default().to_vec();
    let status = ctx.register_procedure(
      "MOD.ARG",
      move |_: &dyn ModuleApi, args: &[&[u8]], out: &mut Vec<u8>| {
        if let Some(a) = args.first() {
          out.extend_from_slice(a);
        }
        out.extend_from_slice(&prefix);
        Ok(())
      },
      None,
    );
    assert_eq!(status, ModuleActionStatus::Success);
  }
}

#[test]
fn test_load_and_execute() -> Void {
  let registry = ModuleRegistry::default();
  assert_eq!(registry.load(&CounterModule, &[]).unwrap(), "counter");
  assert!(!registry.is_empty());

  let proc_ = registry.lookup("MOD.COUNTER").expect("命令应已注册");
  assert_eq!(registry.command_info("MOD.COUNTER").unwrap().arity, -1);

  let api = NoopApi;
  let mut out = Vec::new();
  proc_.execute(&api, &[b"5"], &mut out).unwrap();
  assert_eq!(out, b":5\r\n");
  proc_.execute(&api, &[], &mut out).unwrap();
  assert_eq!(out[4..], b":6\r\n"[..]);
  OK
}

#[test]
fn test_duplicate_and_unload() -> Void {
  let registry = ModuleRegistry::default();
  assert_eq!(registry.load(&CounterModule, &[]).unwrap(), "counter");
  // 同名模块拒绝，错误信息携带注册表中已记录的模块名
  assert!(matches!(
    registry.load(&CounterModule, &[]),
    Err(Error::AlreadyExists(ref name)) if name == "counter"
  ));
  // 注册拒绝不残留：原模块的命令路由与元信息不受影响
  assert!(registry.lookup("MOD.COUNTER").is_some());
  assert!(registry.command_info("MOD.COUNTER").is_some());
  assert_eq!(registry.list().len(), 1);

  assert!(registry.unload("counter"));
  assert!(registry.lookup("MOD.COUNTER").is_none());
  assert!(registry.command_info("MOD.COUNTER").is_none());
  assert!(registry.is_empty());
  // 重复卸载返回 false
  assert!(!registry.unload("counter"));
  // 卸载后可重新加载
  assert_eq!(registry.load(&CounterModule, &[]).unwrap(), "counter");
  OK
}

#[test]
fn test_bad_modules() -> Void {
  let registry = ModuleRegistry::default();
  // 失败加载不残留任何注册信息
  assert!(matches!(
    registry.load(&BareModule, &[]),
    Err(Error::InvalidRegistrationInfo)
  ));
  assert!(matches!(
    registry.load(&EmptyNameModule, &[]),
    Err(Error::InvalidRegistrationInfo)
  ));
  assert!(registry.is_empty());
  assert!(registry.list().is_empty());
  OK
}

#[test]
fn test_register_guards() -> Void {
  let registry = ModuleRegistry::default();
  assert_eq!(registry.load(&GuardModule, &[]).unwrap(), "guard");
  // 未初始化的注册与空命令名均未残留
  assert!(registry.lookup("G.NOINIT").is_none());
  assert!(registry.lookup("G.CMD").is_some());
  OK
}

#[test]
fn test_load_args_binary_safe() -> Void {
  let registry = ModuleRegistry::default();
  // 加载参数含非法 UTF-8 与 NUL 字节，验证二进制安全透传
  registry.load(&ArgsModule, &[&[0xff, 0x00, b'b']]).unwrap();
  let proc_ = registry.lookup("MOD.ARG").expect("命令应已注册");
  let api = NoopApi;
  let mut out = Vec::new();
  proc_.execute(&api, &[b"x"], &mut out).unwrap();
  assert_eq!(out, b"x\xff\x00b");
  // 未传加载参数时为空切片
  let registry2 = ModuleRegistry::default();
  registry2.load(&ArgsModule, &[]).unwrap();
  let mut out = Vec::new();
  registry2
    .lookup("MOD.ARG")
    .unwrap()
    .execute(&api, &[b"y"], &mut out)
    .unwrap();
  assert_eq!(out, b"y");
  OK
}

#[test]
fn test_registry_concurrency() -> Void {
  fn assert_send_sync<T: Send + Sync>() {}
  assert_send_sync::<ModuleRegistry>();

  let registry = Arc::new(ModuleRegistry::default());
  registry.load(&CounterModule, &[]).unwrap();

  let stop = Arc::new(AtomicBool::new(false));
  let handles = (0..4)
    .map(|_| {
      let registry = registry.clone();
      let stop = stop.clone();
      thread::spawn(move || {
        // 卸载窗口内命中与否均合法，仅验证并发下无数据竞争与死锁
        while !stop.load(Ordering::Relaxed) {
          if let Some(p) = registry.lookup("MOD.COUNTER") {
            let mut out = Vec::new();
            p.execute(&NoopApi, &[], &mut out).unwrap();
          }
          let _ = registry.list().len();
          let _ = registry.command_info("MOD.COUNTER");
        }
      })
    })
    .collect::<Vec<_>>();

  // 主线程反复卸载/装载，与读线程并发竞争写锁
  for _ in 0..200 {
    assert!(registry.unload("counter"));
    thread::yield_now();
    registry.load(&CounterModule, &[]).unwrap();
  }
  stop.store(true, Ordering::Relaxed);
  for h in handles {
    h.join().unwrap();
  }
  OK
}

#[test]
fn test_procedure_outlives_unload() -> Void {
  let registry = ModuleRegistry::default();
  registry.load(&CounterModule, &[]).unwrap();
  let proc_ = registry.lookup("MOD.COUNTER").unwrap();
  let api = NoopApi;
  let mut out = Vec::new();
  proc_.execute(&api, &[b"7"], &mut out).unwrap();
  assert_eq!(out, b":7\r\n");

  // 卸载仅解除路由：旧 Arc 句柄由引用计数保持存活，仍可安全执行
  assert!(registry.unload("counter"));
  assert!(registry.lookup("MOD.COUNTER").is_none());
  out.clear();
  proc_.execute(&api, &[b"3"], &mut out).unwrap();
  assert_eq!(out, b":10\r\n");
  OK
}

#[test]
fn test_parse_module_spec() -> Void {
  assert_eq!(
    parse_module_spec("/lib/mod.so arg1 arg2").unwrap(),
    ("/lib/mod.so", vec!["arg1", "arg2"])
  );
  assert_eq!(
    parse_module_spec("\"/my libs/mod.so\" a b").unwrap(),
    ("/my libs/mod.so", vec!["a", "b"])
  );
  // tab 与混合空白作分隔符
  assert_eq!(
    parse_module_spec("/a.so\tb \t c").unwrap(),
    ("/a.so", vec!["b", "c"])
  );
  // 闭合引号与参数间不要求分隔符（宽松解析）
  assert_eq!(parse_module_spec("\"/a b\"x").unwrap(), ("/a b", vec!["x"]));
  // 参数中的引号为字面字符，无转义处理
  assert_eq!(
    parse_module_spec("/a.so \"x y").unwrap(),
    ("/a.so", vec!["\"x", "y"])
  );
  // 引号内路径两端空白去除、内部保留
  assert_eq!(
    parse_module_spec("\"  a  b  \"").unwrap(),
    ("a  b", Vec::new())
  );
  // 畸形输入一律拒绝：空串、全空白、未闭合、空路径、仅引号
  assert!(parse_module_spec("").is_err());
  assert!(parse_module_spec("  \t ").is_err());
  assert!(parse_module_spec("\"unclosed").is_err());
  assert!(parse_module_spec("\"   \"").is_err());
  assert!(parse_module_spec("\"\"").is_err());
  assert!(parse_module_spec("\"").is_err());
  // 裁剪与分词同用 ASCII 空白集：垂直制表符属于路径内容而非分隔符
  assert_eq!(
    parse_module_spec("\u{B}/a.so").unwrap(),
    ("\u{B}/a.so", Vec::new())
  );
  OK
}

#[test]
fn test_resp_encoding() -> Void {
  assert_eq!(RespValue::ok().to_resp(), b"+OK\r\n");
  // 整数边界：零、负数、极值
  assert_eq!(RespValue::Integer(0).to_resp(), b":0\r\n");
  assert_eq!(RespValue::Integer(-7).to_resp(), b":-7\r\n");
  assert_eq!(
    RespValue::Integer(i64::MAX).to_resp(),
    b":9223372036854775807\r\n"
  );
  assert_eq!(
    RespValue::Integer(i64::MIN).to_resp(),
    b":-9223372036854775808\r\n"
  );
  assert_eq!(RespValue::Bulk(None).to_resp(), b"$-1\r\n");
  assert_eq!(
    RespValue::Array(vec![RespValue::Bulk(Some(b"a".to_vec()))]).to_resp(),
    b"*1\r\n$1\r\na\r\n"
  );
  // 空数组与空二进制串
  assert_eq!(RespValue::Array(vec![]).to_resp(), b"*0\r\n");
  assert_eq!(RespValue::Bulk(Some(b"".to_vec())).to_resp(), b"$0\r\n\r\n");
  assert_eq!(RespValue::err("ERR x").to_resp(), b"-ERR x\r\n");
  // 错误回复经 into_result 转 Err
  assert!(RespValue::err("ERR x").into_result().is_err());
  assert!(RespValue::Integer(1).into_result().is_ok());
  OK
}

/// 过程内直接实现 trait（非闭包路径）
struct EchoProc;

impl CustomProcedure for EchoProc {
  fn execute(
    &self,
    _api: &dyn ModuleApi,
    args: &[&[u8]],
    out: &mut Vec<u8>,
  ) -> wedb_module::Result<()> {
    RespValue::Array(
      args
        .iter()
        .map(|a| RespValue::Bulk(Some(a.to_vec())))
        .collect(),
    )
    .write_resp(out);
    Ok(())
  }
}

#[test]
fn test_trait_object_procedure() -> Void {
  struct ProcModule;

  impl Module for ProcModule {
    fn on_load(&self, ctx: &mut wedb_module::ModuleLoadContext<'_>, _args: &[&[u8]]) {
      assert_eq!(ctx.initialize("p", 1), ModuleActionStatus::Success);
      assert_eq!(
        ctx.register_procedure("P.ECHO", EchoProc, None),
        ModuleActionStatus::Success
      );
    }
  }

  let registry = ModuleRegistry::default();
  registry.load(&ProcModule, &[]).unwrap();
  let proc_ = registry.lookup("P.ECHO").unwrap();
  let api = NoopApi;
  let mut out = Vec::new();
  proc_.execute(&api, &[b"x", b"y"], &mut out).unwrap();
  assert_eq!(out, b"*2\r\n$1\r\nx\r\n$1\r\ny\r\n");
  OK
}
