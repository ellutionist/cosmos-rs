//! 错误分类：把 io::Error 归成 Lua 侧可判定的字符串前缀
//!   "verify: ..."    证书校验失败
//!   "handshake: ..." 其它 TLS 握手/协议错误
//!   "io: ..."        底层 socket/IO 错误
//! 已有协议错误（"closed" / "timeout" / "buffer overflow"）不经过这里。

/// 分类一个 IO 错误；rustls 错误被 tokio-rustls 包在 io::Error 里，向下转型取回
pub(crate) fn classify_io(e: &std::io::Error) -> String {
    if let Some(rustls_err) = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        let kind = match rustls_err {
            rustls::Error::InvalidCertificate(_) => "verify",
            _ => "handshake",
        };
        return format!("{kind}: {rustls_err}");
    }
    format!("io: {e}")
}
