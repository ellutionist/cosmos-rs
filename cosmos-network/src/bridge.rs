//! cosmos-network 的 Lua 桥接层。
//!
//! 职责仅限本 crate 无法下沉到 cosmos 的叶子转换：
//! - 本 crate 自有类型的 `FromLua` / `PushLua`（Headers、LuaBytes、ReceivePattern 等），
//!   实现里只组合 cosmos 的基层原语，不写多值压栈逻辑；
//! - TCP 迭代器闭包的 C trampoline（upvalue 挂 userdata）；
//! - 全部 userdata 类型注册。
//!
//! 多返回值/表结构一律由 `#[derive(cosmos::PushLua)]` 与 `&str` / `Vec<u8>` /
//! `Nil` / `LuaStrMap` 等 cosmos 基层类型组合而成，功能模块只构造值。

use cosmos::ffi::{c_char, c_int, lua_State, lua_pushlstring};
use cosmos::userdata::{push_closure, trampoline, try_check, FromLua, LuaStrMap, PushLua};

use crate::http::client::HttpClient;
use crate::http::server::{HttpRequest, HttpServer, HttpServerHandle};
use crate::http::{Headers, LuaBytes};
use crate::tcp::stream::{
    ReceivePattern, Tcp, TcpConn, TcpListener, TcpReader, TcpUntil, TcpWriter, UntilIter,
};
use crate::tcp::tls::{ClientCfg, ServerCfg, Tls, TlsConn, TlsContext, TlsListener};
use crate::websocket::conn::WsConn;
use crate::websocket::Ws;

// ---------------------------------------------------------------------------
// HTTP：Headers / LuaBytes
// ---------------------------------------------------------------------------

impl FromLua for Headers {
    unsafe fn from_lua(lua_state: *mut lua_State, idx: c_int) -> Result<Self, String> {
        let map = LuaStrMap::from_lua(lua_state, idx)?;
        Ok(Headers(
            map.0
                .into_iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v))
                .collect(),
        ))
    }
}

impl PushLua for Headers {
    unsafe fn push_lua(self, lua_state: *mut lua_State) -> i32 {
        LuaStrMap(self.0).push_lua(lua_state)
    }
}

/// Bytes ↔ Lua 字符串（持有 Bytes 零拷贝，push 时拷进 Lua）
impl PushLua for LuaBytes {
    unsafe fn push_lua(self, lua_state: *mut lua_State) -> i32 {
        unsafe { lua_pushlstring(lua_state, self.0.as_ptr() as *const c_char, self.0.len()) };
        1
    }
}

impl FromLua for LuaBytes {
    unsafe fn from_lua(lua_state: *mut lua_State, idx: c_int) -> Result<Self, String> {
        let bytes = <Vec<u8> as FromLua>::from_lua(lua_state, idx)?;
        Ok(LuaBytes(bytes.into()))
    }
}

// ---------------------------------------------------------------------------
// TCP：receive 参数与迭代器闭包
// ---------------------------------------------------------------------------

#[derive(cosmos::FromLua)]
enum ReceiveArg {
    #[lua(type = "number")]
    Size(i64),
    #[lua(type = "string")]
    Pattern(String),
}

impl FromLua for ReceivePattern {
    unsafe fn from_lua(lua_state: *mut lua_State, idx: c_int) -> Result<Self, String> {
        match ReceiveArg::from_lua(lua_state, idx)? {
            ReceiveArg::Size(size) => Ok(ReceivePattern::Size(size.max(0) as usize)),
            ReceiveArg::Pattern(s) => {
                let pat = s.strip_prefix('*').unwrap_or(&s);
                match pat {
                    "a" => Ok(ReceivePattern::All),
                    "l" => Ok(ReceivePattern::Line),
                    "L" => Ok(ReceivePattern::LineKeep),
                    _ => Err(format!(
                        "unsupported receive pattern '{s}' (expected '*l', '*L', '*a' or a number)"
                    )),
                }
            }
        }
    }
}

/// receiveuntil 返回的迭代器：压栈时构造 TcpUntil userdata 并包成 Lua 闭包
/// （upvalue = userdata）。Lua 用法：local iter = reader:receiveuntil("--"); iter()
impl PushLua for UntilIter {
    unsafe fn push_lua(self, lua_state: *mut lua_State) -> i32 {
        unsafe { push_closure::<TcpUntil>(lua_state, until_call, self.0) };
        1
    }
}

/// lua_upvalueindex(1)：5.1 宏展开为 LUA_GLOBALSINDEX - 1
const UPVALUE_1: c_int = cosmos::ffi::LUA_GLOBALSINDEX - 1;

/// 闭包本体：从 upvalue 取 TcpUntil（RefMut 移进 future 持有），
/// 像异步方法一样 yield 出去等读取结果
unsafe extern "C" fn until_call(lua_state: *mut lua_State) -> c_int {
    trampoline(lua_state, |lua_state| unsafe {
        let mut this = try_check::<TcpUntil>(lua_state, UPVALUE_1)?;
        Ok(cosmos::coroutine::yield_with(
            lua_state,
            Box::pin(async move { this.next().await }),
        ))
    })
}

// ---------------------------------------------------------------------------
// TLS：ctx 参数
// ---------------------------------------------------------------------------

impl FromLua for ClientCfg {
    unsafe fn from_lua(lua_state: *mut lua_State, idx: c_int) -> Result<Self, String> {
        let mut ctx = try_check::<TlsContext>(lua_state, idx)?;
        ctx.client_config().map(ClientCfg)
    }
}

impl FromLua for ServerCfg {
    unsafe fn from_lua(lua_state: *mut lua_State, idx: c_int) -> Result<Self, String> {
        let mut ctx = try_check::<TlsContext>(lua_state, idx)?;
        ctx.server_config().map(ServerCfg)
    }
}

// ---------------------------------------------------------------------------
// 类型注册
// ---------------------------------------------------------------------------

/// 注册全部网络 userdata 类型。
pub(crate) fn register(machine: &mut cosmos::Machine) {
    machine.register::<Tcp>();
    machine.register::<TcpConn>();
    machine.register::<TcpReader>();
    machine.register::<TcpWriter>();
    machine.register::<TcpUntil>();
    machine.register::<TcpListener>();

    machine.register::<Tls>();
    machine.register::<TlsContext>();
    machine.register::<TlsConn>();
    machine.register::<TlsListener>();

    machine.register::<HttpClient>();
    machine.register::<HttpServer>();
    machine.register::<HttpServerHandle>();
    machine.register::<HttpRequest>();

    machine.register::<Ws>();
    machine.register::<WsConn>();
}
