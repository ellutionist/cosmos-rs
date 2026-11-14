//! HTTP 模块：client 与 server 分开实现（client.rs / server.rs），底层统一走 hyper。
//!
//! 入口同时包含共享 URL 解析；这里定义客户端连接流（明文/TLS 统一枚举）、Headers、
//! 二进制 body 包装 LuaBytes。Lua 转换与注册集中在 crate::bridge。

pub(crate) mod body;
pub(crate) mod client;
pub(crate) mod file;
pub(crate) mod server;
pub(crate) mod url;

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
#[derive(Clone, Default)]
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
