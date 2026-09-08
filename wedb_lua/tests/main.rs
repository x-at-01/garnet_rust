use std::{cell::RefCell, sync::Arc, time::Duration};

use aok::{OK, Void};
use gxhash::HashMap;
use wedb_lua::{DEFAULT_TIMEOUT, Error, ScriptEngine, sha1_hex};
use wedb_module::{ModuleApi, RespValue};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 测试用存储：SET/GET/DEL/EXISTS 最小实现，内存字典（内部可变）
struct MemApi(RefCell<HashMap<Vec<u8>, Vec<u8>>>);

impl ModuleApi for MemApi {
  fn call(&self, cmd: &str, args: &[&[u8]]) -> wedb_module::Result<RespValue> {
    let mut map = self.0.borrow_mut();
    if cmd.eq_ignore_ascii_case("SET") {
      map.insert(args[0].to_vec(), args[1].to_vec());
      Ok(RespValue::ok())
    } else if cmd.eq_ignore_ascii_case("GET") {
      Ok(RespValue::Bulk(map.get(args[0]).cloned()))
    } else if cmd.eq_ignore_ascii_case("DEL") {
      Ok(RespValue::Integer(map.remove(args[0]).is_some() as i64))
    } else if cmd.eq_ignore_ascii_case("EXISTS") {
      Ok(RespValue::Integer(map.contains_key(args[0]) as i64))
    } else {
      Ok(RespValue::err(format!("ERR unknown command '{cmd}'")))
    }
  }
}

fn engine() -> ScriptEngine {
  ScriptEngine::new(DEFAULT_TIMEOUT).unwrap()
}

fn api() -> Arc<dyn ModuleApi> {
  Arc::new(MemApi(RefCell::new(HashMap::default())))
}

const SCRIPT: &str = r#"
redis.call('SET', KEYS[1], ARGV[1])
return redis.call('GET', KEYS[1])
"#;

#[test]
fn test_eval_roundtrip() -> Void {
  let e = engine();
  let api = api();
  let reply = e.eval(SCRIPT, &[b"greeting"], &[b"hello"], api).unwrap();
  assert_eq!(reply, RespValue::Bulk(Some(b"hello".to_vec())));
  OK
}

#[test]
fn test_eval_sha_cache() -> Void {
  let e = engine();
  let api = api();
  assert_eq!(e.script_count(), 0);

  let sha = e.script_load(SCRIPT);
  assert_eq!(sha.unwrap(), sha1_hex(SCRIPT.as_bytes()));

  // EVALSHA 命中
  let sha = &sha1_hex(SCRIPT.as_bytes());
  let reply = e.eval_sha(sha, &[b"k"], &[b"v"], api.clone()).unwrap();
  assert_eq!(reply, RespValue::Bulk(Some(b"v".to_vec())));

  // SCRIPT EXISTS
  let missing = "0".repeat(40);
  assert_eq!(e.script_exists(&[sha, &missing]), vec![true, false]);

  // SCRIPT FLUSH 后 NOSCRIPT
  e.script_flush();
  assert!(matches!(
    e.eval_sha(sha, &[], &[], api),
    Err(Error::ScriptNotFound)
  ));
  OK
}

#[test]
fn test_return_value_kinds() -> Void {
  let e = engine();
  let api = api();

  // 整数
  assert_eq!(
    e.eval("return 42", &[], &[], api.clone()).unwrap(),
    RespValue::Integer(42)
  );
  // 浮点截断为整数
  assert_eq!(
    e.eval("return 3.7", &[], &[], api.clone()).unwrap(),
    RespValue::Integer(3)
  );
  // 布尔
  assert_eq!(
    e.eval("return true", &[], &[], api.clone()).unwrap(),
    RespValue::Integer(1)
  );
  assert_eq!(
    e.eval("return false", &[], &[], api.clone()).unwrap(),
    RespValue::Bulk(None)
  );
  // nil
  assert_eq!(
    e.eval("return nil", &[], &[], api.clone()).unwrap(),
    RespValue::Bulk(None)
  );
  // 数组
  assert_eq!(
    e.eval("return {1, 'a', true}", &[], &[], api.clone())
      .unwrap(),
    RespValue::Array(vec![
      RespValue::Integer(1),
      RespValue::Bulk(Some(b"a".to_vec())),
      RespValue::Integer(1),
    ])
  );
  // 状态回复
  assert_eq!(
    e.eval("return redis.status_reply('OK')", &[], &[], api)
      .unwrap(),
    RespValue::Status(b"OK".to_vec())
  );
  OK
}

#[test]
fn test_error_paths() -> Void {
  let e = engine();
  let api = api();

  // redis.call 抛出宿主错误
  let err = e
    .eval("return redis.call('NOSUCH', 'x')", &[], &[], api.clone())
    .unwrap_err();
  assert!(err.to_string().contains("unknown command"), "{err}");

  // pcall 捕获为 {err=...}
  assert_eq!(
    e.eval(
      "local r = redis.pcall('NOSUCH', 'x') return r.err",
      &[],
      &[],
      api.clone()
    )
    .unwrap(),
    RespValue::Bulk(Some(b"ERR unknown command 'NOSUCH'".to_vec()))
  );

  // 语法错误
  assert!(e.eval("return )", &[], &[], api).is_err());
  OK
}

#[test]
fn test_sandbox_and_helpers() -> Void {
  let e = engine();
  let api = api();

  // print 与 io 被禁用
  assert_eq!(
    e.eval("return type(print)", &[], &[], api.clone()).unwrap(),
    RespValue::Bulk(Some(b"nil".to_vec()))
  );
  assert_eq!(
    e.eval("return type(io)", &[], &[], api.clone()).unwrap(),
    RespValue::Bulk(Some(b"nil".to_vec()))
  );

  // os 保留 safe 时间函数：clock, date, difftime, time
  assert_eq!(
    e.eval("return type(os)", &[], &[], api.clone()).unwrap(),
    RespValue::Bulk(Some(b"table".to_vec()))
  );
  assert_eq!(
    e.eval("return type(os.clock)", &[], &[], api.clone())
      .unwrap(),
    RespValue::Bulk(Some(b"function".to_vec()))
  );
  assert_eq!(
    e.eval("return type(os.date)", &[], &[], api.clone())
      .unwrap(),
    RespValue::Bulk(Some(b"function".to_vec()))
  );
  assert_eq!(
    e.eval("return type(os.difftime)", &[], &[], api.clone())
      .unwrap(),
    RespValue::Bulk(Some(b"function".to_vec()))
  );
  assert_eq!(
    e.eval("return type(os.time)", &[], &[], api.clone())
      .unwrap(),
    RespValue::Bulk(Some(b"function".to_vec()))
  );
  // os.execute 与 os.getenv 不存在
  assert_eq!(
    e.eval("return type(os.execute)", &[], &[], api.clone())
      .unwrap(),
    RespValue::Bulk(Some(b"nil".to_vec()))
  );

  // debug 库保留只读 traceback 与 info，无危险修改函数
  assert_eq!(
    e.eval("return type(debug)", &[], &[], api.clone()).unwrap(),
    RespValue::Bulk(Some(b"table".to_vec()))
  );
  assert_eq!(
    e.eval("return type(debug.traceback)", &[], &[], api.clone())
      .unwrap(),
    RespValue::Bulk(Some(b"function".to_vec()))
  );
  assert_eq!(
    e.eval("return type(debug.info)", &[], &[], api.clone())
      .unwrap(),
    RespValue::Bulk(Some(b"function".to_vec()))
  );
  assert_eq!(
    e.eval("return type(debug.setlocal)", &[], &[], api.clone())
      .unwrap(),
    RespValue::Bulk(Some(b"nil".to_vec()))
  );

  // buffer 内存缓冲区能力
  let buf_res = e
    .eval(
      "local b = buffer.create(4) buffer.writeu32(b, 0, 0x12345678) return buffer.readu32(b, 0)",
      &[],
      &[],
      api.clone(),
    )
    .unwrap();
  assert_eq!(buf_res, RespValue::Integer(0x12345678));

  // cjson 编码与解码
  let json_res = e
    .eval(
      "return cjson.decode(cjson.encode({a=1, b='test'}))['b']",
      &[],
      &[],
      api.clone(),
    )
    .unwrap();
  assert_eq!(json_res, RespValue::Bulk(Some(b"test".to_vec())));

  // unpack 兼容性
  let unpack_res = e
    .eval(
      "local a, b = unpack({10, 20}) return a + b",
      &[],
      &[],
      api.clone(),
    )
    .unwrap();
  assert_eq!(unpack_res, RespValue::Integer(30));

  // error_reply
  let err_res = e
    .eval(
      "return redis.error_reply('my error')",
      &[],
      &[],
      api.clone(),
    )
    .unwrap();
  assert_eq!(err_res, RespValue::Error(b"ERR my error".to_vec()));

  // 禁止在脚本中污染全局变量
  let glob_err = e
    .eval("my_global_var = 123", &[], &[], api.clone())
    .unwrap_err();
  assert!(
    glob_err
      .to_string()
      .contains("Script attempted to create global variable"),
    "{glob_err}"
  );

  // sha1hex
  let sha = e
    .eval("return redis.sha1hex('sha1')", &[], &[], api.clone())
    .unwrap();
  assert_eq!(
    sha,
    RespValue::Bulk(Some(sha1_hex(b"sha1").into_bytes())),
    "sha1hex 应与 rust 侧一致"
  );

  // KEYS/ARGV 传参
  let n = e
    .eval("return #KEYS + #ARGV", &[b"a", b"b"], &[b"c"], api.clone())
    .unwrap();
  assert_eq!(n, RespValue::Integer(3));

  // os.time 与 os.clock 实际调用测试
  let time_res = e
    .eval(
      "return os.time() > 0 and os.clock() >= 0",
      &[],
      &[],
      api.clone(),
    )
    .unwrap();
  assert_eq!(time_res, RespValue::Integer(1));

  // debug.traceback 实际调用测试
  let tb_res = e
    .eval(
      "return type(debug.traceback('test'))",
      &[],
      &[],
      api.clone(),
    )
    .unwrap();
  assert_eq!(tb_res, RespValue::Bulk(Some(b"string".to_vec())));

  // redis.call 整数参数零拷贝路径测试
  e.eval("redis.call('SET', 'counter', 100)", &[], &[], api.clone())
    .unwrap();
  let val = e
    .eval("return redis.call('GET', 'counter')", &[], &[], api)
    .unwrap();
  assert_eq!(val, RespValue::Bulk(Some(b"100".to_vec())));

  OK
}

#[test]
fn test_timeout() -> Void {
  let e = ScriptEngine::new(Duration::from_millis(80)).unwrap();
  let err = e
    .eval(
      "local s = 0 while true do s = s + 1 end return s",
      &[],
      &[],
      api(),
    )
    .unwrap_err();
  assert!(err.to_string().contains("timed out"), "{err}");
  OK
}

#[test]
fn test_pattern_match_timeout() -> Void {
  // 字符串模式匹配运行在原生代码中，不经过 VM 执行安全点，须由 pattern
  // 安全点强制超时（'.-b' 对长串是平方级扫描，漏配会让脚本永久占住线程）
  let e = ScriptEngine::new(Duration::from_millis(120)).unwrap();
  let err = e
    .eval(
      "return string.find(string.rep('a', 200000), '.-b')",
      &[],
      &[],
      api(),
    )
    .unwrap_err();
  assert!(err.to_string().contains("timed out"), "{err}");
  OK
}

#[test]
fn test_coroutine_timeout() -> Void {
  // coroutine 库开放（对标 Garnet 默认 API 面）：协程内死循环仍受中断钩子超时约束
  let e = ScriptEngine::new(Duration::from_millis(80)).unwrap();
  assert_eq!(
    e.eval("return type(coroutine.wrap)", &[], &[], api())
      .unwrap(),
    RespValue::Bulk(Some(b"function".to_vec()))
  );
  let err = e
    .eval(
      "local c = coroutine.wrap(function() local s = 0 while true do s = s + 1 end end) return c()",
      &[],
      &[],
      api(),
    )
    .unwrap_err();
  assert!(err.to_string().contains("timed out"), "{err}");
  OK
}

#[test]
fn test_number_arg_formatting() -> Void {
  // 数字参数按 Luau "%.14g" 语义转字符串（对标 lua_tostring），而非截断为整数
  let e = engine();
  let api = api();

  let cases: &[(&str, &str)] = &[
    ("3.7", "3.7"),
    ("0.1 + 0.2", "0.3"),
    ("3.0", "3"),
    ("-2.75", "-2.75"),
    ("1e15", "1e+15"),
    ("1e13", "10000000000000"),
    ("0.00001", "1e-05"),
    ("0.0001", "0.0001"),
    ("12345678901234", "12345678901234"),
    // 指数边界：先按 14 位有效数字舍入再决定定点/科学计数（与 glibc %.14g 逐字节一致）
    // 99999999999999.98 舍入到 1e14 后必须转科学计数，而非定点展开
    ("99999999999999.98", "1e+14"),
    ("9.999999999999999e13", "1e+14"),
    ("1e14", "1e+14"),
    ("1e14 + 1", "1e+14"),
    ("-99999999999999.98", "-1e+14"),
    // 舍入进位后恰好落在 1e-4 边界：定点表示
    ("0.0001 - 1e-20", "0.0001"),
    // 14 位有效数字恰为半点：半偶舍入（与 glibc round-half-even 一致）
    ("123456789012345", "1.2345678901234e+14"),
    ("12345678901234.5", "12345678901234"),
    // 负零、极端量级与 2^63
    ("-0.0", "-0"),
    ("0/(-1)", "-0"),
    ("2^63", "9.2233720368548e+18"),
    ("5e-324", "4.9406564584125e-324"),
    ("1.7976931348623157e308", "1.7976931348623e+308"),
  ];
  for (script, want) in cases {
    let set = format!("redis.call('SET', 'nk', {script})");
    e.eval(&set, &[], &[], api.clone()).unwrap();
    let got = e
      .eval("return redis.call('GET', 'nk')", &[], &[], api.clone())
      .unwrap();
    assert_eq!(
      got,
      RespValue::Bulk(Some(want.as_bytes().to_vec())),
      "{script} 应格式化为 {want}"
    );
  }
  OK
}

#[test]
fn test_nil_bulk_reply_is_false() -> Void {
  // redis.call 的 NULL 批量回复折叠为 false（对标 Redis/Garnet），脚本可等值判定
  let e = engine();
  let res = e
    .eval(
      "if redis.call('GET', 'missing') == false then return 'as-false' end return 'as-nil'",
      &[],
      &[],
      api(),
    )
    .unwrap();
  assert_eq!(res, RespValue::Bulk(Some(b"as-false".to_vec())));
  OK
}

#[test]
fn test_global_write_protection() -> Void {
  let e = engine();
  let api = api();

  // rawset 绕过元表的路径同样被只读标志拦截
  let rawset_err = e
    .eval("rawset(_G, 'hello', 'world')", &[], &[], api.clone())
    .unwrap_err();
  assert!(rawset_err.to_string().contains("readonly"), "{rawset_err}");

  // 篡改 _G 元表也被拦截
  assert!(
    e.eval("setmetatable(_G, nil)", &[], &[], api.clone())
      .is_err()
  );

  // KEYS/ARGV 是唯一允许修改的全局表（对标 Garnet ReadOnlyGlobalTables）
  let res = e
    .eval(
      "table.insert(KEYS, 'fizz') return #KEYS",
      &[b"k"],
      &[],
      api.clone(),
    )
    .unwrap();
  assert_eq!(res, RespValue::Integer(2));

  // table.freeze/isfrozen 被移除：防止脚本冻结 KEYS/ARGV 永久破坏后续运行的重填逻辑
  let res = e.eval("return type(table.freeze)", &[], &[], api).unwrap();
  assert_eq!(res, RespValue::Bulk(Some(b"nil".to_vec())));
  OK
}

#[test]
fn test_sha_case_insensitive() -> Void {
  // EVALSHA/SCRIPT EXISTS 对大写十六进制同样命中（对标 Garnet 转小写重试）
  let e = engine();
  let api = api();
  let sha = e.script_load(SCRIPT).unwrap();
  assert_eq!(sha, sha1_hex(SCRIPT.as_bytes()));

  let upper = sha.to_uppercase();
  let reply = e.eval_sha(&upper, &[b"k"], &[b"v"], api.clone()).unwrap();
  assert_eq!(reply, RespValue::Bulk(Some(b"v".to_vec())));
  assert_eq!(e.script_exists(&[upper.as_str()]), vec![true]);
  OK
}

#[test]
fn test_redis_compat_surface() -> Void {
  let e = engine();
  let api = api();

  // load/loadstring/require 不暴露给脚本（对标 Garnet LoadAndLoadStringNotExposed）
  for name in ["load", "loadstring", "require", "dofile", "print"] {
    let res = e
      .eval(&format!("return type({name})"), &[], &[], api.clone())
      .unwrap();
    assert_eq!(
      res,
      RespValue::Bulk(Some(b"nil".to_vec())),
      "{name} 应被禁用"
    );
  }

  // replicate_commands/setresp/acl_check_cmd 成功返回 true
  for name in ["replicate_commands", "setresp", "acl_check_cmd"] {
    let res = e
      .eval(&format!("return redis.{name}()"), &[], &[], api.clone())
      .unwrap();
    assert_eq!(res, RespValue::Integer(1), "redis.{name} 应返回 true");
  }

  // 不支持的调试/复制接口返回固定错误（对标 Garnet）
  for (name, msg) in [
    ("set_repl", "set_repl"),
    ("breakpoint", "breakpoint"),
    ("debug", "debug"),
  ] {
    let err = e
      .eval(&format!("return redis.{name}()"), &[], &[], api.clone())
      .unwrap_err();
    assert!(
      err
        .to_string()
        .contains(&format!("redis.{msg} is not supported")),
      "{err}"
    );
  }

  // sha1hex 对数字参数按 "%.14g" 转字符串后再摘要
  let res = e.eval("return redis.sha1hex(123)", &[], &[], api).unwrap();
  assert_eq!(res, RespValue::Bulk(Some(sha1_hex(b"123").into_bytes())));
  OK
}

#[test]
fn test_cyclic_table_return() -> Void {
  // 自引用表返回应报错而非递归打爆调用栈 abort 进程
  let e = engine();
  let err = e
    .eval("local t = {} t[1] = t return t", &[], &[], api())
    .unwrap_err();
  assert!(err.to_string().contains("nesting"), "{err}");
  OK
}

#[test]
fn test_cjson_depth_guards() -> Void {
  let e = engine();
  let api = api();

  // 循环引用表 encode：报错而非递归打爆调用栈 abort 进程
  let err = e
    .eval(
      "local t = {} t[1] = t return cjson.encode(t)",
      &[],
      &[],
      api.clone(),
    )
    .unwrap_err();
  assert!(err.to_string().contains("nesting"), "{err}");

  // 深嵌套表 encode 同样受深度限制
  assert!(
    e.eval(
      "local t = {} for i = 1, 100000 do t = {t} end return cjson.encode(t)",
      &[],
      &[],
      api.clone(),
    )
    .is_err()
  );

  // 深嵌套 JSON decode：解析前预检深度，报错而非 sonic DOM 解析栈溢出
  let deep = format!("cjson.decode('{}1{}')", "[".repeat(200), "]".repeat(200));
  assert!(e.eval(&deep, &[], &[], api.clone()).is_err());

  // 限内深度正常解码
  let ok = e
    .eval(
      "return cjson.decode('[[1], [2]]')[2][1]",
      &[],
      &[],
      api.clone(),
    )
    .unwrap();
  assert_eq!(ok, RespValue::Integer(2));

  // 字符串字面量中的括号不干扰预扫描的深度统计
  let ok = e
    .eval(r#"return #cjson.decode('{"a":"[[[["}').a"#, &[], &[], api)
    .unwrap();
  assert_eq!(ok, RespValue::Integer(4));
  OK
}

#[test]
fn test_oom_recovery() -> Void {
  // 内存配额打满：错误信息还原为 "not enough memory"（对标 Redis 透传），
  // 连续两次 OOM 脚本后引擎状态可恢复、仍可正常执行
  let e = engine();
  let api = api();
  let err1 = e
    .eval("return #string.rep('a', 300000000)", &[], &[], api.clone())
    .unwrap_err();
  assert!(err1.to_string().contains("not enough memory"), "{err1}");
  let err2 = e
    .eval(
      // buffer 分配路径（与 string.rep 不同）单次大块分配，毫秒级确定性 OOM；
      // 递增型循环会在配额边缘遭遇 GC 抖动，退化为超时而非 OOM，不可用作断言
      "local b = buffer.create(300000000) return buffer.len(b)",
      &[],
      &[],
      api.clone(),
    )
    .unwrap_err();
  assert!(err2.to_string().contains("not enough memory"), "{err2}");
  let ok = e.eval("return 1 + 1", &[], &[], api).unwrap();
  assert_eq!(ok, RespValue::Integer(2));
  OK
}

#[test]
fn test_redis_call_arg_types() -> Void {
  let e = engine();
  let api = api();

  // call：bool/nil/表参数一律抛错（仅字符串与数字合法）
  for arg in ["true", "nil", "{}"] {
    let err = e
      .eval(
        &format!("redis.call('SET', 'k', {arg})"),
        &[],
        &[],
        api.clone(),
      )
      .unwrap_err();
    assert!(
      err.to_string().contains("must be strings or integers"),
      "{arg}: {err}"
    );
  }

  // pcall：参数类型错误折叠为 {err=...}，不抛出（含完全缺参）
  for script in ["redis.pcall('SET', 'k', true)", "redis.pcall()"] {
    let res = e
      .eval(
        &format!("local r = {script} return r.err"),
        &[],
        &[],
        api.clone(),
      )
      .unwrap();
    assert_eq!(
      res,
      RespValue::Bulk(Some(
        b"ERR lua redis lib command arguments must be strings or integers".to_vec()
      ))
    );
  }
  OK
}

#[test]
fn test_return_table_precedence() -> Void {
  // 同时含 ok/err 字段时 ok 优先（对标 Redis luaToRedisReply 的检查顺序）
  let e = engine();
  let api = api();
  assert_eq!(
    e.eval("return {ok = 'good', err = 'bad'}", &[], &[], api.clone())
      .unwrap(),
    RespValue::Status(b"good".to_vec())
  );
  assert_eq!(
    e.eval("return {err = 'bad'}", &[], &[], api).unwrap(),
    RespValue::Error(b"bad".to_vec())
  );
  OK
}
