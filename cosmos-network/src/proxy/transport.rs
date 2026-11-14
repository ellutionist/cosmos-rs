//! 仅在 TLS 层和确有预读时装箱；明文 TCP 保持内联。
use hyper_util::client::legacy::connect::{Connected, Connection};
use std::{
    io::Cursor,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
pub(super) enum Io {
    Tcp(TcpStream),
    Tls(Box<TlsStream<Io>>),
    Prefix(Cursor<Vec<u8>>, Box<Io>),
}
impl Io {
    pub(super) fn prefixed(self, bytes: Vec<u8>) -> Self {
        if bytes.is_empty() {
            self
        } else {
            Self::Prefix(Cursor::new(bytes), Box::new(self))
        }
    }
}
impl AsyncRead for Io {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(io) => Pin::new(io).poll_read(cx, buf),
            Self::Tls(io) => Pin::new(io).poll_read(cx, buf),
            Self::Prefix(prefix, io) => {
                let at = prefix.position() as usize;
                if at < prefix.get_ref().len() {
                    let n = buf.remaining().min(prefix.get_ref().len() - at);
                    buf.put_slice(&prefix.get_ref()[at..at + n]);
                    prefix.set_position((at + n) as u64);
                    Poll::Ready(Ok(()))
                } else {
                    Pin::new(io).poll_read(cx, buf)
                }
            }
        }
    }
}
impl AsyncWrite for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(io) => Pin::new(io).poll_write(cx, buf),
            Self::Tls(io) => Pin::new(io).poll_write(cx, buf),
            Self::Prefix(_, io) => Pin::new(io).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(io) => Pin::new(io).poll_flush(cx),
            Self::Tls(io) => Pin::new(io).poll_flush(cx),
            Self::Prefix(_, io) => Pin::new(io).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(io) => Pin::new(io).poll_shutdown(cx),
            Self::Tls(io) => Pin::new(io).poll_shutdown(cx),
            Self::Prefix(_, io) => Pin::new(io).poll_shutdown(cx),
        }
    }
}
pub(crate) struct Transport {
    pub(super) io: Io,
    pub(super) h2: bool,
    pub(super) forward: bool,
}
impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}
impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}
impl Connection for Transport {
    fn connected(&self) -> Connected {
        let connected = Connected::new().proxy(self.forward);
        if self.h2 {
            connected.negotiated_h2()
        } else {
            connected
        }
    }
}
