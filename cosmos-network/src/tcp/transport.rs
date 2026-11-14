//! TLS 底层传输与升级前的预读字节；已有 TCP 与 Unix 连接共享握手流程。

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

enum Stream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

pub(crate) struct PlainIo {
    stream: Stream,
    prefix: Vec<u8>,
    position: usize,
}

impl PlainIo {
    pub(crate) fn tcp(stream: TcpStream) -> Self {
        Self {
            stream: Stream::Tcp(stream),
            prefix: Vec::new(),
            position: 0,
        }
    }

    #[cfg(unix)]
    pub(super) fn unix(stream: tokio::net::UnixStream) -> Self {
        Self {
            stream: Stream::Unix(stream),
            prefix: Vec::new(),
            position: 0,
        }
    }

    pub(super) fn with_prefix(mut self, prefix: Vec<u8>) -> Self {
        self.prefix = prefix;
        self.position = 0;
        self
    }
}

impl AsyncRead for PlainIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.position < this.prefix.len() {
            let len = buf.remaining().min(this.prefix.len() - this.position);
            buf.put_slice(&this.prefix[this.position..this.position + len]);
            this.position += len;
            if this.position == this.prefix.len() {
                this.prefix = Vec::new();
                this.position = 0;
            }
            return Poll::Ready(Ok(()));
        }
        match &mut this.stream {
            Stream::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
            #[cfg(unix)]
            Stream::Unix(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for PlainIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut self.get_mut().stream {
            Stream::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
            #[cfg(unix)]
            Stream::Unix(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().stream {
            Stream::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(unix)]
            Stream::Unix(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().stream {
            Stream::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(unix)]
            Stream::Unix(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}
