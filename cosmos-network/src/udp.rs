//! UDP 报文收发；共享句柄不跨 await 持有 userdata 或 RefCell 借用。

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use cosmos::userdata::LuaBytes;
use tokio::net::{lookup_host, UdpSocket};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::deadline::with_deadline;
use crate::error::classify_io;

// 采用 IPv4 / IPv6 均可表示的报文上限，不支持 IPv6 jumbogram。
const MAX_DATAGRAM: usize = 65_507;

#[derive(Clone)]
pub(crate) struct Udp(Rc<Inner>);

struct Inner {
    socket: RefCell<Option<Rc<UdpSocket>>>,
    closed: Notify,
    timeout: Cell<Option<Duration>>,
    connected: bool,
}

fn port(value: f64) -> Result<u16, String> {
    if !value.is_finite() || value.fract() != 0.0 || !(0.0..=u16::MAX as f64).contains(&value) {
        return Err("invalid port (expected an integer in 0..65535)".into());
    }
    Ok(value as u16)
}

fn timeout(ms: f64) -> Result<Option<Duration>, String> {
    if !ms.is_finite() || ms > 1e12 {
        return Err("invalid timeout (ms)".into());
    }
    Ok((ms > 0.0).then(|| Duration::from_secs_f64(ms / 1000.0)))
}

fn receive_limit(size: Option<f64>) -> Result<usize, String> {
    let size = size.unwrap_or(MAX_DATAGRAM as f64);
    if !size.is_finite() || size.fract() != 0.0 || !(1.0..=MAX_DATAGRAM as f64).contains(&size) {
        return Err(format!(
            "invalid receive size (expected an integer in 1..{MAX_DATAGRAM})"
        ));
    }
    Ok(size as usize)
}

fn address(addr: SocketAddr) -> (String, u16) {
    let host = match addr {
        SocketAddr::V6(addr) if addr.scope_id() != 0 => {
            format!("{}%{}", addr.ip(), addr.scope_id())
        }
        _ => addr.ip().to_string(),
    };
    (host, addr.port())
}

async fn timed<T>(
    duration: Option<Duration>,
    operation: impl Future<Output = io::Result<T>>,
) -> Result<T, String> {
    with_deadline(
        duration.map(|duration| Instant::now() + duration),
        operation,
    )
    .await
    .map_err(|_| "timeout".to_owned())?
    .map_err(|error| classify_io(&error))
}

fn sent(count: usize, expected: usize) -> Result<i64, String> {
    if count != expected {
        return Err("io: incomplete datagram".into());
    }
    Ok(count as i64)
}

impl Udp {
    fn from_socket(socket: UdpSocket, timeout: Option<Duration>, connected: bool) -> Self {
        Self(Rc::new(Inner {
            socket: RefCell::new(Some(Rc::new(socket))),
            closed: Notify::new(),
            timeout: Cell::new(timeout),
            connected,
        }))
    }

    fn socket(&self) -> Result<Rc<UdpSocket>, String> {
        self.0
            .socket
            .borrow()
            .clone()
            .ok_or_else(|| "closed".into())
    }

    async fn wait<T>(&self, operation: impl Future<Output = io::Result<T>>) -> Result<T, String> {
        let closed = self.0.closed.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        if self.0.socket.borrow().is_none() {
            return Err("closed".into());
        }
        tokio::select! {
            biased;
            _ = closed => Err("closed".into()),
            result = timed(self.0.timeout.get(), operation) => result,
        }
    }

    async fn packet(&self, size: Option<f64>) -> Result<(Vec<u8>, SocketAddr), String> {
        let socket = self.socket()?;
        let limit = receive_limit(size)?;
        // 接收完整的普通 IP 报文后检查用户上限，不能把截断内容作为成功报文返回。
        let mut buffer = vec![0; 65_536];
        let (count, sender) = self.wait(socket.recv_from(&mut buffer)).await?;
        if count > limit {
            return Err("buffer overflow".into());
        }
        buffer.truncate(count);
        Ok((buffer, sender))
    }
}

#[cosmos::userdata(name = "network.udp")]
impl Udp {
    /// 绑定本地地址；端口 0 由系统分配。超时也作为后续收发的初始配置。
    async fn bind(host: String, number: f64, timeout_ms: Option<f64>) -> Result<Self, String> {
        let number = port(number)?;
        let duration = timeout(timeout_ms.unwrap_or(0.0))?;
        let socket = timed(duration, UdpSocket::bind((host.as_str(), number))).await?;
        Ok(Self::from_socket(socket, duration, false))
    }

    /// 解析默认目标并自动绑定同族本地地址；UDP connect 不代表远端可达。
    async fn connect(host: String, number: f64, timeout_ms: Option<f64>) -> Result<Self, String> {
        let number = port(number)?;
        let duration = timeout(timeout_ms.unwrap_or(0.0))?;
        let socket = timed(duration, async {
            let addresses = lookup_host((host.as_str(), number)).await?;
            let mut failure =
                io::Error::new(io::ErrorKind::AddrNotAvailable, "no UDP address resolved");
            for peer in addresses {
                let local = if peer.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                };
                match UdpSocket::bind(local).await {
                    Ok(socket) => match socket.connect(peer).await {
                        Ok(()) => return Ok(socket),
                        Err(error) => failure = error,
                    },
                    Err(error) => failure = error,
                }
            }
            Err(failure)
        })
        .await?;
        Ok(Self::from_socket(socket, duration, true))
    }

    fn state(&self) -> i64 {
        i64::from(self.0.socket.borrow().is_none())
    }

    /// 毫秒超时，<= 0 清除；只影响之后开始的操作。
    fn settimeout(&self, ms: f64) -> Result<(), String> {
        self.socket()?;
        self.0.timeout.set(timeout(ms)?);
        Ok(())
    }

    fn localaddr(&self) -> Result<(String, u16), String> {
        self.socket()?
            .local_addr()
            .map(address)
            .map_err(|error| classify_io(&error))
    }

    fn peeraddr(&self) -> Result<(String, u16), String> {
        let socket = self.socket()?;
        if !self.0.connected {
            return Err("not connected".into());
        }
        socket
            .peer_addr()
            .map(address)
            .map_err(|error| classify_io(&error))
    }

    /// 一次发送对应一份报文，包括空报文，不循环重试部分发送。
    async fn send(self, data: LuaBytes) -> Result<i64, String> {
        let socket = self.socket()?;
        if !self.0.connected {
            return Err("not connected".into());
        }
        if data.len() > MAX_DATAGRAM {
            return Err("datagram too large".into());
        }
        sent(self.wait(socket.send(&data)).await?, data.len())
    }

    /// 未连接 socket 的指定目标发送；解析与发送共用一个超时。
    async fn sendto(self, data: LuaBytes, host: String, number: f64) -> Result<i64, String> {
        let socket = self.socket()?;
        if self.0.connected {
            return Err("already connected".into());
        }
        let number = port(number)?;
        if data.len() > MAX_DATAGRAM {
            return Err("datagram too large".into());
        }
        let ipv4 = socket
            .local_addr()
            .map_err(|error| classify_io(&error))?
            .is_ipv4();
        let count = self
            .wait(async {
                let target = lookup_host((host.as_str(), number))
                    .await?
                    .find(|addr| addr.is_ipv4() == ipv4)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::AddrNotAvailable,
                            "no address matching UDP socket family",
                        )
                    })?;
                socket.send_to(&data, target).await
            })
            .await?;
        sent(count, data.len())
    }

    /// 接收一份报文；已连接时由内核过滤来源，未连接时接受任意来源。
    async fn receive(self, size: Option<f64>) -> Result<Vec<u8>, String> {
        self.packet(size).await.map(|(data, _)| data)
    }

    async fn receivefrom(self, size: Option<f64>) -> Result<(Vec<u8>, String, u16), String> {
        let (data, sender) = self.packet(size).await?;
        let (host, number) = address(sender);
        Ok((data, host, number))
    }

    fn setbroadcast(&self, enabled: bool) -> Result<bool, String> {
        self.socket()?
            .set_broadcast(enabled)
            .map_err(|error| classify_io(&error))?;
        Ok(true)
    }

    fn broadcast(&self) -> Result<bool, String> {
        self.socket()?
            .broadcast()
            .map_err(|error| classify_io(&error))
    }

    /// 幂等关闭并通知所有等待者；等待中的操作持有最后的底层引用直到退出。
    fn close(&self) {
        self.0.socket.borrow_mut().take();
        self.0.closed.notify_waiters();
    }
}
