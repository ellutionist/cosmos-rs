//! HTTP 模块：client 与 server 分开实现（client.rs / server.rs），底层统一走 hyper。
//!
//! 这里只放两者共用的数据类型：客户端连接流（明文/TLS 统一枚举）、Headers、
//! 二进制 body 包装 LuaBytes。Lua 转换与注册集中在 crate::bridge。

pub(crate) mod client;
pub(crate) mod server;

use std::pin::Pin;
use std::task::{Context, Poll};

use hyper_util::client::legacy::connect::{Connected, Connection};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::tcp::tls::TlsIo;

/// HTTP 客户端的底层连接：明文或 TLS，统一实现 AsyncRead/AsyncWrite，
/// 让 hyper 的连接池对两种传输只有一种类型
#[allow(
    clippy::large_enum_variant,
    reason = "inline connection streams avoid an extra Box allocation per TLS connection"
)]
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
pub(crate) struct Headers(pub(crate) Vec<(String, String)>);

impl Headers {
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 二进制安全的 Lua 字符串 ↔ Bytes（body 可能是任意字节）。
/// 持有 Bytes 是为了多次取值时零拷贝（clone 只是引用计数）
#[derive(Default)]
pub(crate) struct LuaBytes(pub(crate) bytes::Bytes);

/// HTTP 版本 → Lua 侧数字（10 / 11 / 20 / 30）
fn version_num(v: hyper::Version) -> i64 {
    match v {
        hyper::Version::HTTP_10 => 10,
        hyper::Version::HTTP_11 => 11,
        hyper::Version::HTTP_2 => 20,
        hyper::Version::HTTP_3 => 30,
        _ => 0,
    }
}
