//! 顺序协议使用的 socket：复用流读写，并在失败或取消后丢弃连接。

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use cosmos::userdata::{LuaBytes, Nil};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tokio_rustls::TlsAcceptor;

use crate::deadline::{timeout_from_ms, with_deadline};
use crate::error::classify_io;
use crate::tcp::stream::{Duplex, ReceiveOutcome, ReceivePattern, State};
use crate::tcp::tls::{client_server_name, handshake_client, ClientCfg, ServerCfg, TlsIo};

pub(crate) struct Socket {
    stream: Option<Duplex>,
    host: Option<String>,
    timeout: Option<Duration>,
    max_buf: Option<usize>,
}

impl Socket {
    fn tcp(stream: TcpStream) -> Self {
        Self {
            stream: Some(Duplex::tcp(
                stream,
                Rc::new(Cell::new(State::Connected)),
                None,
                None,
            )),
            ..Self::new()
        }
    }

    #[cfg(unix)]
    fn unix(stream: tokio::net::UnixStream) -> Self {
        Self {
            stream: Some(Duplex::unix(
                stream,
                Rc::new(Cell::new(State::Connected)),
                None,
                None,
            )),
            ..Self::new()
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.timeout.map(|timeout| Instant::now() + timeout)
    }

    fn take_plain(&mut self) -> Result<crate::tcp::transport::PlainIo, String> {
        let stream = self.stream.as_ref().ok_or("closed")?;
        if stream.is_tls() {
            return Err("TLS is already enabled".into());
        }
        self.stream.take().unwrap().into_plain()
    }

    fn install_tls(&mut self, stream: TlsIo) {
        self.stream = Some(Duplex::tls(
            stream,
            Rc::new(Cell::new(State::Connected)),
            self.timeout,
            self.max_buf,
        ));
    }
}

/// 未完成的异步操作包括取消路径；离开时只保留成功且仍可读写的连接。
struct Operation<'a> {
    stream: &'a mut Option<Duplex>,
    keep: bool,
}

impl<'a> Operation<'a> {
    fn new(stream: &'a mut Option<Duplex>) -> Result<Self, String> {
        if !stream.as_ref().is_some_and(Duplex::connected) {
            if let Some(mut closed) = stream.take() {
                closed.abort();
            }
            return Err("closed".into());
        }
        Ok(Self {
            stream,
            keep: false,
        })
    }

    fn io(&mut self) -> &mut Duplex {
        self.stream.as_mut().unwrap()
    }

    fn finish(mut self, success: bool) {
        self.keep = success && self.stream.as_ref().is_some_and(Duplex::connected);
    }
}

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if !self.keep {
            if let Some(mut stream) = self.stream.take() {
                stream.abort();
            }
        }
    }
}

#[cosmos::userdata(name = "network.socket")]
impl Socket {
    fn new() -> Self {
        Self {
            stream: None,
            host: None,
            timeout: None,
            max_buf: None,
        }
    }

    fn state(&self) -> i64 {
        if self.stream.as_ref().is_some_and(Duplex::connected) {
            0
        } else {
            1
        }
    }

    /// 配置连接、读写及 TLS 升级的超时，单位毫秒；<= 0 清除超时。
    fn settimeout(&mut self, ms: i64) {
        self.timeout = timeout_from_ms(ms);
        if let Some(stream) = &mut self.stream {
            stream.settimeout(ms);
        }
    }

    fn setbufsize(&mut self, size: i64) {
        self.max_buf = (size > 0).then_some(size as usize);
        if let Some(stream) = &mut self.stream {
            stream.setbufsize(size);
        }
    }

    async fn connect(&mut self, host: String, port: u16) -> Result<i64, String> {
        if self.stream.is_some() {
            return Err("already connected".into());
        }
        let stream =
            match with_deadline(self.deadline(), TcpStream::connect((host.as_str(), port))).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => return Err(classify_io(&error)),
                Err(()) => return Err("timeout".into()),
            };
        self.stream = Some(Duplex::tcp(
            stream,
            Rc::new(Cell::new(State::Connected)),
            self.timeout,
            self.max_buf,
        ));
        self.host = Some(host);
        Ok(1)
    }

    async fn connect_unix(&mut self, path: String) -> Result<i64, String> {
        if self.stream.is_some() {
            return Err("already connected".into());
        }
        #[cfg(unix)]
        {
            let stream =
                match with_deadline(self.deadline(), tokio::net::UnixStream::connect(path)).await {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(error)) => return Err(classify_io(&error)),
                    Err(()) => return Err("timeout".into()),
                };
            self.stream = Some(Duplex::unix(
                stream,
                Rc::new(Cell::new(State::Connected)),
                self.timeout,
                self.max_buf,
            ));
            self.host = None;
            Ok(1)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err("Unix domain sockets are not supported on this platform".into())
        }
    }

    async fn send(&mut self, data: LuaBytes) -> Result<i64, String> {
        let mut operation = Operation::new(&mut self.stream)?;
        let result = operation.io().send(data).await;
        operation.finish(result.is_ok());
        result
    }

    async fn receive(&mut self, pattern: Option<ReceivePattern>) -> ReceiveOutcome {
        let Ok(mut operation) = Operation::new(&mut self.stream) else {
            return ReceiveOutcome::Closed(Nil, "closed", Vec::new());
        };
        let result = operation.io().receive(pattern).await;
        operation.finish(matches!(&result, ReceiveOutcome::Data(_)));
        result
    }

    /// 在同一连接上升级 TLS。没有显式名称时，使用 TCP connect 的主机名。
    async fn starttls(
        &mut self,
        host: Option<String>,
        ctx: Option<ClientCfg>,
    ) -> Result<bool, String> {
        let host = host
            .or_else(|| self.host.clone())
            .ok_or("TLS server name is required")?;
        let server_name = client_server_name(&host)?;
        let plain = self.take_plain()?;
        let stream = handshake_client(plain, server_name, self.deadline(), ctx).await?;
        self.install_tls(stream);
        Ok(true)
    }

    /// 服务端升级复用已有证书配置，保留底层缓冲中已预读的 ClientHello。
    async fn starttls_server(&mut self, ctx: ServerCfg) -> Result<bool, String> {
        let plain = self.take_plain()?;
        let stream =
            match with_deadline(self.deadline(), TlsAcceptor::from(ctx.0).accept(plain)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => return Err(classify_io(&error)),
                Err(()) => return Err("timeout".into()),
            };
        self.install_tls(TlsIo::Server(stream));
        Ok(true)
    }

    async fn close(&mut self) -> Result<i64, String> {
        self.stream.take().ok_or("closed")?.close().await;
        Ok(1)
    }

    /// 协议出错或连接池淘汰时立即丢弃底层连接，不发送应用层关闭报文。
    fn abort(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            stream.abort();
        }
    }

    async fn listen(
        host: String,
        port: u16,
        timeout_ms: Option<i64>,
    ) -> Result<SocketListener, String> {
        let listener = TcpListener::bind((host.as_str(), port))
            .await
            .map_err(|error| classify_io(&error))?;
        Ok(SocketListener {
            listener: Some(Listener::Tcp(listener)),
            timeout: timeout_ms.and_then(timeout_from_ms),
        })
    }

    async fn listen_unix(path: String, timeout_ms: Option<i64>) -> Result<SocketListener, String> {
        #[cfg(unix)]
        {
            let listener =
                tokio::net::UnixListener::bind(path).map_err(|error| classify_io(&error))?;
            Ok(SocketListener {
                listener: Some(Listener::Unix(listener)),
                timeout: timeout_ms.and_then(timeout_from_ms),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (path, timeout_ms);
            Err("Unix domain sockets are not supported on this platform".into())
        }
    }
}

enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

pub(crate) struct SocketListener {
    listener: Option<Listener>,
    timeout: Option<Duration>,
}

#[cosmos::userdata(name = "network.socket.listener")]
impl SocketListener {
    fn port(&self) -> Option<i64> {
        match self.listener.as_ref()? {
            Listener::Tcp(listener) => listener
                .local_addr()
                .ok()
                .map(|address| address.port() as i64),
            #[cfg(unix)]
            Listener::Unix(_) => None,
        }
    }

    async fn accept(&mut self) -> Result<Socket, String> {
        let listener = self.listener.as_ref().ok_or("closed")?;
        let accept = async {
            match listener {
                Listener::Tcp(listener) => listener
                    .accept()
                    .await
                    .map(|(stream, _)| Socket::tcp(stream)),
                #[cfg(unix)]
                Listener::Unix(listener) => listener
                    .accept()
                    .await
                    .map(|(stream, _)| Socket::unix(stream)),
            }
        };
        match with_deadline(self.timeout.map(|timeout| Instant::now() + timeout), accept).await {
            Ok(Ok(socket)) => Ok(socket),
            Ok(Err(error)) => Err(classify_io(&error)),
            Err(()) => Err("timeout".into()),
        }
    }

    fn close(&mut self) {
        self.listener.take();
    }
}
