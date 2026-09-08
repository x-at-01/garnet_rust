//! 脚本引擎：Luau 沙箱 + 字节码缓存 (对标 Garnet libs/server/Lua 与 Redis EVAL/EVALSHA/SCRIPT 命令族)
//!
//! - 沙箱仅加载安全标准库，剪裁 OS 保留 clock/date/difftime/time，debug 保留只读 traceback/info，禁用 print/io/package 与 load 族入口
//! - 预注入 `redis` 全局表与 `KEYS`/`ARGV`；全局表设只读并挂写保护元表，脚本内新建/篡改全局（含 rawset）一律报错
//! - 编译后的脚本按 SHA-1 缓存为 Luau 字节码；运行期通过中断 (InterruptHooks) 精准控制超时，
//!   执行安全点与字符串模式匹配安全点共用同一采样器

use std::{
  borrow::Cow,
  cell::Cell,
  fmt::{self, Write as _},
  rc::Rc,
  str,
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
  time::Duration,
};

use luau::{
  Bytecode, Compiler, Error as LuauError, Function, InterruptAction, InterruptHooks, InterruptMode,
  Lua, LuaRef, LuaString, MultiValue, StdLib, Table, Value,
};
use sonic_rs::JsonNumberTrait as _;
use wedb_module::{ModuleApi, RespValue};
use whasher::{GxPapayaMap, new_papaya_map};

use crate::{
  error::{Error, Result},
  value::{multi_to_resp, resp_to_lua, sha1_hex},
};

/// 默认脚本超时 (5秒)
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// 默认脚本内存上限 (256MB)
pub const DEFAULT_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// `redis.call/pcall` 参数类型错误的统一提示
const ERR_BAD_ARG: &str = "ERR lua redis lib command arguments must be strings or integers";

/// cjson.encode 嵌套超限提示（对标 Lua CJSON 的 excessive nesting 防护）
const ERR_JSON_NESTING: &str = "Cannot serialise, excessive nesting depth";

/// cjson.decode 允许的最大 JSON 嵌套深度。
///
/// sonic-rs 的 DOM 解析路径没有递归深度防护（其 serde 侧闸门不覆盖
/// `from_slice::<Value>`），深嵌套输入会直接栈溢出 abort 进程，故解析前
/// 先做 O(n) 字节扫描预检，超限拒绝。
const MAX_JSON_PARSE_DEPTH: usize = 128;

/// 预扫描 JSON 的最大结构嵌套深度（跳过字符串字面量与转义），是否在限内
fn json_depth_ok(bytes: &[u8]) -> bool {
  let mut depth = 0usize;
  let mut in_str = false;
  let mut escaped = false;
  for &b in bytes {
    if in_str {
      if escaped {
        escaped = false;
      } else if b == b'\\' {
        escaped = true;
      } else if b == b'"' {
        in_str = false;
      }
      continue;
    }
    match b {
      b'"' => in_str = true,
      b'{' | b'[' => {
        depth += 1;
        if depth > MAX_JSON_PARSE_DEPTH {
          return false;
        }
      }
      // 非法 JSON 的孤立闭合符交给后续解析器报错，此处防下溢
      b'}' | b']' => depth = depth.saturating_sub(1),
      _ => {}
    }
  }
  true
}

/// ModuleApi 未注入错误提示
const ERR_API_NOT_INJECTED: &str = "ERR ModuleApi not injected";

/// 脚本超时错误提示
const ERR_TIMEOUT: &str = "ERR script timed out, consider raising the timeout limit";

/// 禁用危险命令提示
const ERR_SET_REPL_UNSUPPORTED: &str = "ERR redis.set_repl is not supported";
const ERR_BREAKPOINT_UNSUPPORTED: &str = "ERR redis.breakpoint is not supported";
const ERR_DEBUG_UNSUPPORTED: &str = "ERR redis.debug is not supported";

/// Redis 日志级别：调试
const LOG_DEBUG: i64 = 0;
/// Redis 日志级别：详细
const LOG_VERBOSE: i64 = 1;
/// Redis 日志级别：通知
const LOG_NOTICE: i64 = 2;
/// Redis 日志级别：警告
const LOG_WARNING: i64 = 3;

/// 兼容的 Redis 版本号
const REDIS_VERSION: &str = "7.2.0";
/// 兼容的 Redis 版本编码 (0x00070200)
const REDIS_VERSION_NUM: i64 = 0x0007_0200;

/// 数字参数内联缓冲长度（itoa i64 最长 20 字节，"%.14g" 最长约 24 字节）
const NUM_BUF_LEN: usize = 32;

/// `%.14g` 的有效数字位数
const SIG_DIGITS: usize = 14;

/// cjson.encode 表转换的最大递归深度（循环引用/深嵌套表会打爆调用栈）
const MAX_JSON_DEPTH: usize = 32;

/// 每 N 个执行安全点采样一次时钟，摊薄热循环下的中断开销
const INTERRUPT_SAMPLE: u32 = 64;

/// 超时采样器：执行与模式匹配安全点共用
struct TimeoutSampler {
  /// 单调时钟基准（引擎创建时刻），免疫 NTP 墙钟回拨
  uptime_base: coarsetime::Instant,
  deadline_ms: Arc<AtomicU64>,
  tick: Cell<u32>,
}

impl TimeoutSampler {
  /// 超时则报错中断脚本；每 N 个安全点才读一次时钟，摊薄热循环下的中断开销
  fn check(&self) -> luau::Result<()> {
    let n = self.tick.get().wrapping_add(1);
    self.tick.set(n);
    if n.is_multiple_of(INTERRUPT_SAMPLE) {
      let d = self.deadline_ms.load(Ordering::Relaxed);
      if self.uptime_base.elapsed().as_millis() > d {
        return Err(LuauError::runtime(ERR_TIMEOUT));
      }
    }
    Ok(())
  }
}

/// Luau 脚本引擎：单会话/单线程持有
pub struct ScriptEngine {
  lua: Lua,
  compiler: Compiler,
  /// sha1hex -> 预编译字节码
  chunks: GxPapayaMap<String, Bytecode>,
  timeout: Duration,
  /// 单调时钟基准，deadline_ms 以此起算
  uptime_base: coarsetime::Instant,
  deadline_ms: Arc<AtomicU64>,
}

// 在单线程/Thread-per-core 模型下，ScriptEngine 在线程内部独立运行；
// 若放入 Mutex 跨线程转移，其内部独占拥有的 Luau 状态无线程竞争。
unsafe impl Send for ScriptEngine {}

impl ScriptEngine {
  /// 创建沙箱引擎；`timeout` 为单次脚本运行上限
  pub fn new(timeout: Duration) -> Result<Self> {
    // 1. 标准库选择：加载安全基础库与 Luau 增强库
    let libs = StdLib::BASE
      | StdLib::COROUTINE
      | StdLib::TABLE
      | StdLib::STRING
      | StdLib::MATH
      | StdLib::UTF8
      | StdLib::BIT32
      | StdLib::BUFFER
      | StdLib::OS
      | StdLib::DEBUG;
    let mut lua = Lua::new_with_libs(libs)?;

    // 2. 内存上限
    lua.set_memory_limit(DEFAULT_MEMORY_LIMIT);

    // 3. 安装中断钩子（超时与死循环控制；中断回调挂在全局状态上，协程内同样生效）。
    // 字符串模式匹配（string.find/gsub/match 等）在原生代码中运行，不经过 VM 执行
    // 安全点，须单独挂 pattern 采样，否则恶意模式 + 长文本（如 '.-b' 对长串的
    // 平方级扫描）可绕过超时长期占住线程
    let deadline_ms = Arc::new(AtomicU64::new(u64::MAX));
    let uptime_base = coarsetime::Instant::now();
    let sampler = Rc::new(TimeoutSampler {
      uptime_base,
      deadline_ms: deadline_ms.clone(),
      tick: Cell::new(0),
    });
    let exec_sampler = Rc::clone(&sampler);
    let pattern_sampler = Rc::clone(&sampler);
    lua.set_interrupt_handler(
      InterruptHooks::new()
        .set_mode(InterruptMode::Continuous)
        .on_execution(move |_| exec_sampler.check().map(|()| InterruptAction::Continue))
        .on_pattern(move |_| pattern_sampler.check()),
    );

    // 4. 剪裁与沙箱配置（遵循 Redis / Garnet 沙箱原则）
    {
      let globals = lua.globals()?;

      // 4.1 os 库：保留安全的时间/日期相关函数（clock, date, difftime, time），设为只读防篡改
      if let Ok(os_lib) = globals.get::<Table>("os") {
        let safe_os = lua.create_table_with_capacity(0, 4)?;
        for fn_name in ["clock", "date", "difftime", "time"] {
          if let Ok(f) = os_lib.get::<Function>(fn_name) {
            safe_os.set(fn_name, f)?;
          }
        }
        safe_os.set_readonly(true);
        globals.set("os", safe_os)?;
      }

      // 4.2 debug 库：Luau 的 debug 仅含只读的 info 与 traceback，无破坏状态的 setupvalue/setlocal
      // 保留用于调用栈分析与排错，设为只读
      if let Ok(debug_lib) = globals.get::<Table>("debug") {
        let safe_debug = lua.create_table_with_capacity(0, 2)?;
        if let Ok(traceback_fn) = debug_lib.get::<Function>("traceback") {
          safe_debug.set("traceback", traceback_fn)?;
        }
        if let Ok(info_fn) = debug_lib.get::<Function>("info") {
          safe_debug.set("info", info_fn)?;
        }
        safe_debug.set_readonly(true);
        globals.set("debug", safe_debug)?;
      }

      // 4.3 彻底禁用危险全局函数与沙箱逃逸入口
      for name in [
        "print",
        "io",
        "package",
        "require",
        "dofile",
        "loadfile",
        "module",
        "load",
        "loadstring",
        "import",
        "setfenv",
        "getfenv",
      ] {
        globals.set(name, Value::Nil)?;
      }

      // 4.4 string 库：禁用 string.dump（禁止导出字节码），其余设为只读
      if let Ok(string_lib) = globals.get::<Table>("string") {
        string_lib.set("dump", Value::Nil)?;
        string_lib.set_readonly(true);
      }

      // 4.5 各基础库设为只读，防止脚本内猴子补丁 (Monkey-patching) 污染环境
      if let Ok(math_lib) = globals.get::<Table>("math") {
        math_lib.set_readonly(true);
      }
      if let Ok(buffer_lib) = globals.get::<Table>("buffer") {
        buffer_lib.set_readonly(true);
      }
      if let Ok(utf8_lib) = globals.get::<Table>("utf8") {
        utf8_lib.set_readonly(true);
      }

      // 4.6 提供 bit32 的别名 bit（只读），兼容 Redis 脚本
      if let Ok(bit32_lib) = globals.get::<Table>("bit32") {
        let bit_table = lua.create_table()?;
        bit32_lib.for_each(|k: Value<'_>, v: Value<'_>| bit_table.set(k, v))?;
        bit_table.set_readonly(true);
        globals.set("bit", bit_table)?;
        bit32_lib.set_readonly(true);
      }

      // 4.7 提供 unpack（等价于 table.unpack），并将 table 设为只读；
      // 移除 Luau 专有的 freeze/isfrozen（Lua 5.4/Garnet 无此接口，
      // 且脚本一旦冻结 KEYS/ARGV 会永久破坏每次运行前的重填逻辑）
      if let Ok(table_lib) = globals.get::<Table>("table") {
        if let Ok(unpack_fn) = table_lib.get::<Function>("unpack") {
          globals.set("unpack", unpack_fn)?;
        }
        table_lib.set("freeze", Value::Nil)?;
        table_lib.set("isfrozen", Value::Nil)?;
        table_lib.set_readonly(true);
      }

      // 4.8 注入 cjson 库（参考 Garnet）
      let cjson_table = lua.create_table_with_capacity(0, 2)?;
      cjson_table.set(
        "encode",
        lua.create_function(|lua, mut args| {
          let val: Value<'_> = args.next()?;
          let json_val = lua_to_json(&val, 0)?;
          let json_str = sonic_rs::to_string(&json_val)
            .map_err(|e| LuauError::runtime(format!("cjson encode error: {e}")))?;
          args.finish(lua.create_string(json_str)?)
        })?,
      )?;
      cjson_table.set(
        "decode",
        lua.create_function(|lua, mut args| {
          let s: LuaString<'_> = args.next()?;
          let bytes = s.as_bytes();
          if !json_depth_ok(bytes) {
            return Err(LuauError::runtime(ERR_JSON_NESTING));
          }
          let json_val: sonic_rs::Value = sonic_rs::from_slice(bytes)
            .map_err(|e| LuauError::runtime(format!("cjson decode error: {e}")))?;
          let lua_val = json_ref_to_lua(lua, &json_val)?;
          args.finish(lua_val)
        })?,
      )?;
      cjson_table.set_readonly(true);
      globals.set("cjson", cjson_table)?;

      // 4.9 注入 redis 全局表（单次注册，长久复用）
      let redis_table = lua.create_table_with_capacity(0, 16)?;
      redis_table.set(
        "call",
        lua.create_function(|lua, mut args| {
          let (cmd, argv) = parse_redis_call_args(&mut args)?;
          let reply = call_api(lua, &cmd, &argv)?;
          let lua_val =
            resp_to_lua(lua, reply, true).map_err(|e| LuauError::runtime(e.to_string()))?;
          args.finish(lua_val)
        })?,
      )?;
      redis_table.set(
        "pcall",
        lua.create_function(|lua, mut args| {
          let lua_val = match parse_redis_call_args(&mut args) {
            Ok((cmd, argv)) => match call_api(lua, &cmd, &argv) {
              Ok(reply) => {
                resp_to_lua(lua, reply, false).map_err(|e| LuauError::runtime(e.to_string()))?
              }
              Err(e) => err_table(lua, &e.to_string())?,
            },
            // pcall 语义：参数类型错误不抛出，折叠为 {err=...}
            Err(_) => err_table(lua, ERR_BAD_ARG)?,
          };
          args.finish(lua_val)
        })?,
      )?;
      redis_table.set(
        "sha1hex",
        lua.create_function(|lua, mut args| {
          let mut buf = [0u8; NUM_BUF_LEN];
          let sha = match args.next::<Value<'_>>()? {
            Value::String(s) => sha1_hex(s.as_bytes()),
            Value::Integer(n) => {
              let len = usize::from(write_i64(n, &mut buf));
              sha1_hex(&buf[..len])
            }
            Value::Number(n) => {
              let len = usize::from(write_g14(n, &mut buf));
              sha1_hex(&buf[..len])
            }
            _ => return Err(LuauError::runtime(ERR_BAD_ARG)),
          };
          args.finish(lua.create_string(sha)?)
        })?,
      )?;
      redis_table.set(
        "status_reply",
        lua.create_function(|lua, mut args| {
          let s: LuaString<'_> = args.next()?;
          let t = lua.create_table_with_capacity(0, 1)?;
          t.raw_set("ok", s)?;
          args.finish(Value::Table(t))
        })?,
      )?;
      redis_table.set(
        "error_reply",
        lua.create_function(|lua, mut args| {
          let s: LuaString<'_> = args.next()?;
          let bytes = s.as_bytes();
          let err_val = if bytes.starts_with(b"ERR ") {
            s
          } else {
            let mut buf = Vec::with_capacity(4 + bytes.len());
            buf.extend_from_slice(b"ERR ");
            buf.extend_from_slice(bytes);
            lua.create_string(&buf)?
          };
          let t = lua.create_table_with_capacity(0, 1)?;
          t.raw_set("err", err_val)?;
          args.finish(Value::Table(t))
        })?,
      )?;
      redis_table.set(
        "log",
        lua.create_function(|_lua, mut args| {
          let level: i64 = args.next().unwrap_or(LOG_VERBOSE);
          let msg_val: Option<LuaString<'_>> = args.next().ok();
          let msg = msg_val
            .as_ref()
            .map(|s| String::from_utf8_lossy(s.as_bytes()))
            .unwrap_or_default();
          match level {
            LOG_DEBUG => log::debug!("{msg}"),
            LOG_VERBOSE => log::info!("{msg}"),
            LOG_NOTICE => log::warn!("{msg}"),
            LOG_WARNING => log::error!("{msg}"),
            _ => log::info!("{msg}"),
          }
          args.finish(())
        })?,
      )?;
      redis_table.set(
        "replicate_commands",
        lua.create_function(|_lua, args| args.finish(true))?,
      )?;
      redis_table.set(
        "setresp",
        lua.create_function(|_lua, args| args.finish(true))?,
      )?;
      redis_table.set(
        "acl_check_cmd",
        lua.create_function(|_lua, args| args.finish(true))?,
      )?;
      redis_table.set(
        "set_repl",
        lua.create_function(|_lua, _args| Err(LuauError::runtime(ERR_SET_REPL_UNSUPPORTED)))?,
      )?;
      redis_table.set(
        "breakpoint",
        lua.create_function(|_lua, _args| Err(LuauError::runtime(ERR_BREAKPOINT_UNSUPPORTED)))?,
      )?;
      redis_table.set(
        "debug",
        lua.create_function(|_lua, _args| Err(LuauError::runtime(ERR_DEBUG_UNSUPPORTED)))?,
      )?;
      redis_table.set("LOG_DEBUG", LOG_DEBUG)?;
      redis_table.set("LOG_VERBOSE", LOG_VERBOSE)?;
      redis_table.set("LOG_NOTICE", LOG_NOTICE)?;
      redis_table.set("LOG_WARNING", LOG_WARNING)?;
      redis_table.set("REDIS_VERSION", REDIS_VERSION)?;
      redis_table.set("REDIS_VERSION_NUM", REDIS_VERSION_NUM)?;
      redis_table.set_readonly(true);
      globals.set("redis", redis_table)?;

      // 4.10 预建 KEYS/ARGV 占位表：本体常驻全局，运行前重填内容（KEYS/ARGV 可被脚本修改，对标 Garnet）
      let keys_table = lua.create_table()?;
      globals.set("KEYS", keys_table)?;
      let argv_table = lua.create_table()?;
      globals.set("ARGV", argv_table)?;

      // 4.11 全局写保护：__newindex 元表拦截新全局（报错信息对标 Redis），
      // 只读标志兜底 rawset/setmetatable 等绕过元表的路径
      let guard = lua.create_table_with_capacity(0, 1)?;
      guard.set(
        "__newindex",
        lua.create_function(|_lua, mut args| {
          let _table: Value<'_> = args.next()?;
          let key: Value<'_> = args.next()?;
          let key_str = match key {
            Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
            _ => format!("{key:?}"),
          };
          Err(LuauError::runtime(format!(
            "ERR Script attempted to create global variable '{key_str}'"
          )))
        })?,
      )?;
      guard.set_readonly(true);
      globals.set_metatable(Some(&guard))?;
      globals.set_readonly(true);
    }

    let compiler = Compiler::new().set_optimization_level(2);

    Ok(Self {
      lua,
      compiler,
      chunks: new_papaya_map(),
      timeout,
      uptime_base,
      deadline_ms,
    })
  }

  /// 编译脚本或从缓存获取字节码
  fn get_or_compile(&self, script: &str) -> Result<(String, Bytecode)> {
    let sha = sha1_hex(script.as_bytes());
    if let Some(bytecode) = self.chunks.pin().get(&sha).cloned() {
      return Ok((sha, bytecode));
    }
    let bytecode = self.compiler.compile(script)?;
    self.chunks.pin().insert(sha.clone(), bytecode.clone());
    Ok((sha, bytecode))
  }

  /// 编译脚本（命中缓存则直接返回），返回 SHA-1 十六进制
  pub fn script_load(&self, script: &str) -> Result<String> {
    let (sha, _) = self.get_or_compile(script)?;
    Ok(sha)
  }

  /// 检查一组 SHA-1 是否已缓存，大小写不敏感 (对标 SCRIPT EXISTS)
  pub fn script_exists(&self, shas: &[&str]) -> Vec<bool> {
    let pin = self.chunks.pin();
    shas
      .iter()
      .map(|s| pin.contains_key(norm_sha(s).as_ref()))
      .collect()
  }

  /// 清空脚本缓存 (对标 SCRIPT FLUSH)
  pub fn script_flush(&self) {
    self.chunks.pin().clear();
  }

  /// 缓存的脚本数
  pub fn script_count(&self) -> usize {
    self.chunks.pin().len()
  }

  /// 执行脚本正文 (对标 EVAL)
  pub fn eval(
    &self,
    script: &str,
    keys: &[&[u8]],
    argv: &[&[u8]],
    api: Arc<dyn ModuleApi>,
  ) -> Result<RespValue> {
    let (sha, bytecode) = self.get_or_compile(script)?;
    self.run_bytecode(&sha, bytecode, keys, argv, api)
  }

  /// 按 SHA-1 执行缓存脚本，大小写不敏感 (对标 EVALSHA)
  pub fn eval_sha(
    &self,
    sha: &str,
    keys: &[&[u8]],
    argv: &[&[u8]],
    api: Arc<dyn ModuleApi>,
  ) -> Result<RespValue> {
    let sha = norm_sha(sha);
    let bytecode = self
      .chunks
      .pin()
      .get(sha.as_ref())
      .cloned()
      .ok_or(Error::ScriptNotFound)?;
    self.run_bytecode(&sha, bytecode, keys, argv, api)
  }

  /// 执行预编译字节码
  fn run_bytecode(
    &self,
    name: &str,
    bytecode: Bytecode,
    keys: &[&[u8]],
    argv: &[&[u8]],
    api: Arc<dyn ModuleApi>,
  ) -> Result<RespValue> {
    // 1. 注入 KEYS/ARGV：全局表只读，但 KEYS/ARGV 本体可写（对标 Garnet），宿主每次运行前清空重填，零额外分配
    let globals = self.lua.globals()?;
    let keys_table: Table<'_> = globals.raw_get("KEYS")?;
    fill_seq(&keys_table, keys, &self.lua)?;
    let argv_table: Table<'_> = globals.raw_get("ARGV")?;
    fill_seq(&argv_table, argv, &self.lua)?;

    // 2. 注入 ModuleApi 并设定超时截止（单调时钟，免疫墙钟回拨）；此后到运行结束只做不可失败操作，保证现场必然清理
    self.lua.set_app_data(api);
    let now = self.uptime_base.elapsed().as_millis();
    self.deadline_ms.store(
      now.saturating_add(self.timeout.as_millis() as u64),
      Ordering::Relaxed,
    );

    // 3. 加载字节码并执行（超时由中断钩子强制）
    let call_result: luau::Result<MultiValue<'_>> =
      self.lua.load_bytecode(bytecode).set_name(name).call(());

    // 4. 不可失败清理：复位截止时间、移除注入的 ModuleApi（KEYS/ARGV 留待下次运行覆盖）
    self.deadline_ms.store(u64::MAX, Ordering::Relaxed);
    self.lua.remove_app_data::<Arc<dyn ModuleApi>>();

    // 5. 结果与错误规范化（错误统一映射为 RESP 错误文本）
    match call_result {
      Ok(vals) => multi_to_resp(vals),
      Err(e) => {
        let msg = match e {
          LuauError::Interrupted => ERR_TIMEOUT.to_string(),
          LuauError::CallbackError { cause, .. } => {
            strip_error_prefix(&cause.to_string()).to_string()
          }
          LuauError::RuntimeError(s) => strip_error_prefix(&s).to_string(),
          // 内存配额打满：还原原文（对标 Redis 透传 "not enough memory"）
          LuauError::MemoryError(m) => m,
          other => other.to_string(),
        };
        Err(Error::Reply(msg))
      }
    }
  }
}

/// SHA 大小写归一化：全小写时零分配（对标 Garnet 在 miss 时转小写重试）
fn norm_sha(sha: &str) -> Cow<'_, str> {
  if sha.bytes().any(|b| b.is_ascii_uppercase()) {
    Cow::Owned(sha.to_ascii_lowercase())
  } else {
    Cow::Borrowed(sha)
  }
}

/// 清空并按序填充 KEYS/ARGV 表（容量复用）
fn fill_seq(t: &Table<'_>, items: &[&[u8]], lua: &Lua) -> luau::Result<()> {
  t.clear()?;
  for (i, item) in items.iter().enumerate() {
    t.raw_seti(i + 1, lua.create_string(item)?)?;
  }
  Ok(())
}

/// pcall 语义的错误折叠表 {err=...}
fn err_table<'lua>(lua: LuaRef<'lua>, msg: &str) -> luau::Result<Value<'lua>> {
  let t = lua.create_table_with_capacity(0, 1)?;
  t.raw_set("err", lua.create_string(msg)?)?;
  Ok(Value::Table(t))
}

/// 经宿主 ModuleApi 执行一条命令（redis.call/pcall 共用）
fn call_api<'lua>(
  lua: LuaRef<'lua>,
  cmd: &LuaString<'lua>,
  argv: &[CallArg<'lua>],
) -> luau::Result<RespValue> {
  let arg_refs: Vec<&[u8]> = argv.iter().map(CallArg::as_bytes).collect();
  let api_ref = lua
    .app_data_ref::<Arc<dyn ModuleApi>>()
    .ok_or_else(|| LuauError::runtime(ERR_API_NOT_INJECTED))?;
  api_ref
    .call(&cmd.to_str_lossy(), &arg_refs)
    .map_err(|e| LuauError::runtime(e.to_string()))
}

/// redis.call/pcall 参数暂存项（零堆分配）
enum CallArg<'lua> {
  Str(LuaString<'lua>),
  Num([u8; NUM_BUF_LEN], u8),
}

impl CallArg<'_> {
  #[inline]
  fn as_bytes(&self) -> &[u8] {
    match self {
      Self::Str(s) => s.as_bytes(),
      Self::Num(buf, len) => &buf[..*len as usize],
    }
  }

  /// 整数参数（itoa 快速路径）
  fn num_i64(n: i64) -> Self {
    let mut buf = [0u8; NUM_BUF_LEN];
    let len = write_i64(n, &mut buf);
    Self::Num(buf, len)
  }

  /// Lua 数字参数：整值走整数路径，其余按 Luau 的 "%.14g" 规则（对标 lua_tostring）
  fn num_f64(n: f64) -> Self {
    let mut buf = [0u8; NUM_BUF_LEN];
    let len = write_g14(n, &mut buf);
    Self::Num(buf, len)
  }
}

/// 整数十进制写入，返回有效长度
fn write_i64(n: i64, buf: &mut [u8; NUM_BUF_LEN]) -> u8 {
  let mut it = itoa::Buffer::new();
  let s = it.format(n).as_bytes();
  buf[..s.len()].copy_from_slice(s);
  s.len() as u8
}

/// 按 Luau 的 "%.14g" 规则写入浮点，返回有效长度。
///
/// 遵循 C `%g` 的两段式语义：先按 14 位有效数字做正确舍入（`{:.13e}`，半偶舍入），
/// 再依据舍入后的十进制指数决定定点或科学计数（定点条件 -4 <= exp < 14），最后
/// 去除小数尾零。指数边界值与 glibc 逐字节一致（如 99999999999999.98 舍入到
/// 1e14 后转科学计数 "1e+14"，而非定点展开 "100000000000000"）。
fn write_g14(n: f64, buf: &mut [u8; NUM_BUF_LEN]) -> u8 {
  if n.is_nan() {
    buf[..3].copy_from_slice(b"nan");
    return 3;
  }
  if n.is_infinite() {
    let s: &[u8] = if n < 0.0 { b"-inf" } else { b"inf" };
    buf[..s.len()].copy_from_slice(s);
    return s.len() as u8;
  }
  if n == 0.0 {
    let s: &[u8] = if n.is_sign_negative() { b"-0" } else { b"0" };
    buf[..s.len()].copy_from_slice(s);
    return s.len() as u8;
  }
  // 1e14 以内的整值直接走 itoa，与 %.14g 的定点表示逐字节一致（>= 1e14 时 %.14g 转科学计数法）
  if n.abs() < 1e14 && n.fract() == 0.0 {
    return write_i64(n as i64, buf);
  }
  // 科学计数格式化：正确舍入到 14 位有效数字，形如 "d.ddddddddddddde±exp"
  let mut ebuf = [0u8; NUM_BUF_LEN];
  let len = {
    let mut efmt = FmtBuf {
      buf: &mut ebuf,
      len: 0,
    };
    let _ = write!(efmt, "{:.13e}", n.abs());
    efmt.len
  };
  let Some(e_at) = ebuf[..len].iter().position(|&b| b == b'e') else {
    // {:.13e} 必含 'e'，不可达的防御回退
    return write_i64(n as i64, buf);
  };
  // 尾数数字 d1..d14（ebuf 布局：d1 '.' d2..d14 'e'）
  let mut digits = [0u8; SIG_DIGITS];
  digits[0] = ebuf[0];
  digits[1..].copy_from_slice(&ebuf[2..e_at]);
  // 解析舍入后的十进制指数
  let mut exp = 0i32;
  let mut exp_neg = false;
  for &b in &ebuf[e_at + 1..len] {
    match b {
      b'-' => exp_neg = true,
      b'0'..=b'9' => exp = exp * 10 + i32::from(b - b'0'),
      _ => {}
    }
  }
  if exp_neg {
    exp = -exp;
  }

  let mut out = FmtBuf { buf, len: 0 };
  if n < 0.0 {
    out.push(b"-");
  }
  if (-4..14).contains(&exp) {
    // 定点表示：整数部分 + 去尾零的小数部分
    if exp >= 0 {
      let e = exp as usize;
      out.push(&digits[..=e]);
      if e + 1 < SIG_DIGITS {
        let start = out.len;
        out.push(b".");
        out.push(&digits[e + 1..]);
        out.trim_frac(start);
      }
    } else {
      out.push(b"0");
      let start = out.len;
      out.push(b".");
      for _ in 0..(-exp - 1) {
        out.push(b"0");
      }
      out.push(&digits);
      out.trim_frac(start);
    }
  } else {
    // 科学计数法：尾数去尾零，指数至少两位（e+15 / e-05 / e+308，对标 glibc）
    out.push(&digits[..1]);
    let start = out.len;
    out.push(b".");
    out.push(&digits[1..]);
    out.trim_frac(start);
    out.push(b"e");
    out.push(if exp < 0 { b"-" } else { b"+" });
    let e = exp.unsigned_abs();
    if e < 10 {
      out.push(b"0");
    }
    let mut it = itoa::Buffer::new();
    out.push(it.format(e).as_bytes());
  }
  out.len as u8
}

/// 栈上格式化缓冲：实现 fmt::Write 以规避堆分配
struct FmtBuf<'a> {
  buf: &'a mut [u8],
  len: usize,
}

impl FmtBuf<'_> {
  fn push(&mut self, bytes: &[u8]) {
    let rem = self.buf.len() - self.len;
    let n = bytes.len().min(rem);
    self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
    self.len += n;
  }

  /// 从 start 起去除小数尾零与悬挂小数点（无小数点的整数表示保持原样）
  fn trim_frac(&mut self, start: usize) {
    if !self.buf[start..self.len].contains(&b'.') {
      return;
    }
    while self.len > start && self.buf[self.len - 1] == b'0' {
      self.len -= 1;
    }
    if self.len > start && self.buf[self.len - 1] == b'.' {
      self.len -= 1;
    }
  }
}

impl fmt::Write for FmtBuf<'_> {
  fn write_str(&mut self, s: &str) -> fmt::Result {
    self.push(s.as_bytes());
    Ok(())
  }
}

/// 拆分 redis.call/pcall 参数（仅接受字符串与数字，其余报错）
fn parse_redis_call_args<'lua>(
  args: &mut luau::Arguments<'lua>,
) -> luau::Result<(LuaString<'lua>, Vec<CallArg<'lua>>)> {
  let cmd = match args.next::<Value<'lua>>()? {
    Value::String(s) => s,
    _ => return Err(LuauError::runtime(ERR_BAD_ARG)),
  };
  let mut argv = Vec::with_capacity(args.remaining());
  while args.remaining() > 0 {
    match args.next::<Value<'lua>>()? {
      Value::String(s) => argv.push(CallArg::Str(s)),
      Value::Integer(n) => argv.push(CallArg::num_i64(n)),
      Value::Number(n) => argv.push(CallArg::num_f64(n)),
      _ => return Err(LuauError::runtime(ERR_BAD_ARG)),
    }
  }
  Ok((cmd, argv))
}

/// 去除 Luau 错误前缀（含回调包裹路径下的 memory error 前缀）
fn strip_error_prefix(s: &str) -> &str {
  const RUNTIME_PREFIX: &str = "runtime error: ";
  const CALLBACK_PREFIX: &str = "callback error: ";
  const MEMORY_PREFIX: &str = "memory error: ";

  s.strip_prefix(RUNTIME_PREFIX)
    .or_else(|| s.strip_prefix(CALLBACK_PREFIX))
    .or_else(|| s.strip_prefix(MEMORY_PREFIX))
    .unwrap_or(s)
}

/// sonic-rs Value -> Luau Value
fn json_ref_to_lua<'lua>(lua: LuaRef<'lua>, v: &sonic_rs::Value) -> luau::Result<Value<'lua>> {
  match v.as_ref() {
    sonic_rs::ValueRef::Null => Ok(Value::Nil),
    sonic_rs::ValueRef::Bool(b) => Ok(Value::Boolean(b)),
    sonic_rs::ValueRef::Number(n) => {
      if let Some(i) = n.as_i64() {
        Ok(Value::Integer(i))
      } else if let Some(f) = n.as_f64() {
        Ok(Value::Number(f))
      } else {
        Ok(Value::Nil)
      }
    }
    sonic_rs::ValueRef::String(s) => Ok(Value::String(lua.create_string(s.as_bytes())?)),
    sonic_rs::ValueRef::Array(arr) => {
      let t = lua.create_table_with_capacity(arr.len(), 0)?;
      for (i, item) in arr.iter().enumerate() {
        t.raw_seti(i + 1, json_ref_to_lua(lua, item)?)?;
      }
      Ok(Value::Table(t))
    }
    sonic_rs::ValueRef::Object(obj) => {
      let t = lua.create_table_with_capacity(0, obj.len())?;
      for (k, val) in obj.iter() {
        t.raw_set(k, json_ref_to_lua(lua, val)?)?;
      }
      Ok(Value::Table(t))
    }
  }
}

/// Luau Value -> sonic-rs Value（depth 防循环引用/深嵌套表递归打爆调用栈）
fn lua_to_json(v: &Value<'_>, depth: usize) -> luau::Result<sonic_rs::Value> {
  if depth > MAX_JSON_DEPTH {
    return Err(LuauError::runtime(ERR_JSON_NESTING));
  }
  match v {
    Value::Nil => Ok(sonic_rs::Value::from(())),
    Value::Boolean(b) => Ok(sonic_rs::Value::from(*b)),
    Value::Integer(n) => Ok(sonic_rs::Value::from(*n)),
    Value::Number(n) => {
      if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
        Ok(sonic_rs::Value::from(*n as i64))
      } else {
        sonic_rs::Number::from_f64(*n)
          .map(sonic_rs::Value::from)
          .ok_or_else(|| LuauError::runtime("Invalid float (NaN or Inf)"))
      }
    }
    Value::String(s) => {
      if let Ok(valid_str) = str::from_utf8(s.as_bytes()) {
        Ok(sonic_rs::Value::from(valid_str))
      } else {
        Ok(sonic_rs::Value::from(
          String::from_utf8_lossy(s.as_bytes()).as_ref(),
        ))
      }
    }
    Value::Table(t) => {
      let len = t.raw_len();
      let mut items = Vec::with_capacity(len);
      for val in t.sequence_values::<Value<'_>>() {
        items.push(lua_to_json(&val?, depth + 1)?);
      }
      if !items.is_empty() {
        Ok(sonic_rs::Value::from(items))
      } else {
        let mut obj = sonic_rs::Object::new();
        t.for_each(|k: Value<'_>, v: Value<'_>| {
          match k {
            Value::String(s) => {
              let val = lua_to_json(&v, depth + 1)?;
              if let Ok(valid_str) = str::from_utf8(s.as_bytes()) {
                obj.insert(valid_str, val);
              } else {
                let lossy = String::from_utf8_lossy(s.as_bytes());
                obj.insert(lossy.as_ref(), val);
              }
            }
            Value::Integer(n) => {
              let mut buf = itoa::Buffer::new();
              let val = lua_to_json(&v, depth + 1)?;
              obj.insert(buf.format(n), val);
            }
            _ => {}
          }
          Ok(())
        })?;
        Ok(obj.into_value())
      }
    }
    _ => Ok(sonic_rs::Value::from(())),
  }
}
