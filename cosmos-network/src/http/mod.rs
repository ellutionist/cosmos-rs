//! HTTP 模块：client 与 server 分开实现（client.rs / server.rs），底层统一走 hyper。
//!
//! 这里只放两者共用的部件：客户端连接流（明文/TLS 统一枚举）、Header 表 ↔ Lua
//! table 的转换、二进制 body 的压栈包装。

pub(crate) mod client;
pub(crate) mod server;

use std::pin::Pin;
use std::task::{Context, Poll};

use hyper_util::client::legacy::connect::{Connected, Connection};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use cosmos::ffi::*;
use cosmos::userdata::{FromLua, PushLua};

use crate::tls::TlsIo;

/// HTTP 客户端的底层连接：明文或 TLS，统一实现 AsyncRead/AsyncWrite，
/// 让 hyper 的连接池对两种传输只有一种类型
pub(crate) enum Conn {
    Plain(TcpStream),
    Tls(TlsIo),
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Conn::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Conn::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_flush(cx),
            Conn::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Conn::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// 告诉 hyper 连接池 ALPN 协商结果：h2 连接要单独入池（negotiated_h2）
impl Connection for Conn {
    fn connected(&self) -> Connected {
        match self {
            Conn::Tls(io) if io.negotiated_h2() => Connected::new().negotiated_h2(),
            _ => Connected::new(),
        }
    }
}

/// 头表：保持顺序，名字统一小写
#[derive(Clone, Default)]
pub struct Headers(pub Vec<(String, String)>);

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl FromLua for Headers {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TTABLE {
            return Err(format!("argument {idx}: expected table"));
        }
        let mut out = Vec::new();
        lua_pushnil(L);
        while lua_next(L, idx) != 0 {
            if lua_type(L, -2) == LUA_TSTRING && lua_type(L, -1) == LUA_TSTRING {
                let key = take_string(L, -2);
                let value = take_string(L, -1);
                out.push((key.to_ascii_lowercase(), value));
            }
            lua_settop(L, -2); // 弹掉 value，保留 key 供下一次 lua_next
        }
        Ok(Headers(out))
    }
}

impl PushLua for Headers {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe {
            lua_createtable(L, 0, self.0.len() as c_int);
            for (k, v) in self.0 {
                lua_pushlstring(L, k.as_ptr() as *const c_char, k.len());
                lua_pushlstring(L, v.as_ptr() as *const c_char, v.len());
                lua_settable(L, -3);
            }
        }
        1
    }
}

/// 二进制安全的 Lua 字符串 ↔ Bytes（body 可能是任意字节）。
/// 持有 Bytes 是为了多次取值时零拷贝（clone 只是引用计数）
#[derive(Default)]
pub struct LuaBytes(pub bytes::Bytes);

impl PushLua for LuaBytes {
    fn push_lua(self, L: *mut lua_State) -> i32 {
        unsafe { lua_pushlstring(L, self.0.as_ptr() as *const c_char, self.0.len()) };
        1
    }
}

impl FromLua for LuaBytes {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        if lua_type(L, idx) != LUA_TSTRING {
            return Err(format!("argument {idx}: expected string"));
        }
        Ok(LuaBytes(bytes::Bytes::from(take_bytes(L, idx))))
    }
}

pub(crate) unsafe fn take_string(L: *mut lua_State, idx: c_int) -> String {
    String::from_utf8_lossy(&take_bytes(L, idx)).into_owned()
}

pub(crate) unsafe fn take_bytes(L: *mut lua_State, idx: c_int) -> Vec<u8> {
    let mut len: size_t = 0;
    let p = lua_tolstring(L, idx, &mut len);
    if p.is_null() {
        return Vec::new();
    }
    std::slice::from_raw_parts(p as *const u8, len).to_vec()
}

/// HTTP 版本 → Lua 侧数字（10 / 11 / 20 / 30）
pub(crate) fn version_num(v: hyper::Version) -> i64 {
    match v {
        hyper::Version::HTTP_10 => 10,
        hyper::Version::HTTP_11 => 11,
        hyper::Version::HTTP_2 => 20,
        hyper::Version::HTTP_3 => 30,
        _ => 0,
    }
}

pub unsafe fn register(L: *mut lua_State) {
    client::register(L);
    server::register(L);
}
