//! WebSocket 模块（RFC 6455）：客户端连接 + 服务端升级后的连接对象。
//!
//! 服务端不单独监听端口：HTTP server 的 handler 在选定 path 上返回
//! `{ websocket = function(conn, req) ... end }`，由 HTTP 层完成 101 握手，
//! 之后该连接脱离 HTTP 进入帧循环（见 http/server.rs 的 upgrade_websocket）。
//!
//! Lua 侧：
//!   local Ws = require("cosmos.network.websocket")
//!   local conn = assert(Ws.connect("ws://host:8080/ws", { timeout = 3000 }))
//!   conn:send("hello")                  -- 文本帧（要求合法 UTF-8）
//!   conn:send_binary("\0\1\2")          -- 二进制帧
//!   local op, data = conn:receive()     -- op: "text" / "binary"
//!                                       -- 关闭: nil, "closed"；超时: nil, "timeout"
//!   conn:ping("x")
//!   conn:close(1000, "bye")
//!   conn:state()                        -- 0 打开 / 1 已关闭
//!
//! 收发都是协程内非阻塞操作：send/receive 挂起当前 Lua 协程，
//! 由同一 LocalSet 上的 poll 侧推进（与 tcp/http 一致）。
//! ping 由 fastwebsockets 自动回 pong、close 自动回 close（不暴露给 Lua）。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use fastwebsockets::{
    handshake, FragmentCollector, Frame, OpCode, Payload, WebSocket,
};
use http_body_util::Empty;
use hyper::header::{HeaderName, HeaderValue, CONNECTION, HOST, UPGRADE};
use hyper::upgrade::Upgraded;
use hyper::{Method, Request, Uri};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;
use tokio::time::Instant;

use cosmos::userdata::LuaFunction;

use crate::error::classify_io;
use crate::http::client::default_tls_config;
use crate::http::server::HttpRequest;
use crate::http::{Headers, LuaBytes};
use crate::tcp::{timeout_from_ms, with_deadline};
use crate::tls::{connect_tls_io, with_alpn_client, ClientCfg};

const DEFAULT_MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// 握手完成后的连接统一类型（服务端 Upgraded / 客户端 Upgraded 相同）
type WsStream = FragmentCollector<TokioIo<Upgraded>>;

fn new_stream(mut ws: WebSocket<TokioIo<Upgraded>>, max_message: usize) -> WsStream {
    ws.set_auto_pong(true);
    ws.set_auto_close(true);
    ws.set_max_message_size(max_message);
    FragmentCollector::new(ws)
}

fn payload_bytes(payload: Payload<'_>) -> Bytes {
    match payload {
        Payload::Borrowed(b) => Bytes::copy_from_slice(b),
        Payload::BorrowedMut(b) => Bytes::copy_from_slice(b),
        Payload::Owned(v) => Bytes::from(v),
        Payload::Bytes(b) => b.freeze(),
    }
}

/// receive 的成功结果：("text"|"binary", 数据)，由元组 PushLua 作为两个返回值压栈
type WsMessage = (&'static str, LuaBytes);

/// 连接对象：帧流放在共享槽里，服务端在 handler 协程结束后要能强制关闭，
/// 因此 run_conn 与 userdata 持有同一个 Rc
pub struct WsConn {
    ws: Rc<RefCell<Option<WsStream>>>,
    timeout: Option<Duration>,
}

impl WsConn {
    fn take(&self) -> Result<WsStream, String> {
        self.ws
            .borrow_mut()
            .take()
            .ok_or_else(|| "closed".to_string())
    }

    fn put(&self, ws: WsStream) {
        *self.ws.borrow_mut() = Some(ws);
    }

    fn deadline(&self) -> Option<Instant> {
        self.timeout.map(|d| Instant::now() + d)
    }

    /// 写帧：写超时/出错后流状态不可信，不再放回（连接视为关闭）
    async fn write_frame(&self, frame: Frame<'_>) -> Result<(), String> {
        let mut ws = self.take()?;
        let deadline = self.deadline();
        match with_deadline(deadline, ws.write_frame(frame)).await {
            Ok(Ok(())) => {
                self.put(ws);
                Ok(())
            }
            Ok(Err(err)) => Err(err.to_string()),
            Err(()) => Err("timeout".to_string()),
        }
    }
}

#[cosmos::userdata(name = "network.websocket.conn")]
impl WsConn {
    /// 0 打开 / 1 已关闭
    fn state(&self) -> i64 {
        if self.ws.borrow().is_none() {
            1
        } else {
            0
        }
    }

    /// 毫秒；<= 0 清除超时。只影响之后的 send/send_binary/receive/ping/close
    fn settimeout(&mut self, ms: i64) {
        self.timeout = timeout_from_ms(ms);
    }

    /// 文本帧；Lua 字符串必须能按 UTF-8 解码（否则用 send_binary）
    async fn send(&mut self, data: Vec<u8>) -> Result<(), String> {
        if std::str::from_utf8(&data).is_err() {
            return Err("send: payload is not valid UTF-8 (use send_binary)".to_string());
        }
        self.write_frame(Frame::text(Payload::Owned(data))).await
    }

    async fn send_binary(&mut self, data: Vec<u8>) -> Result<(), String> {
        self.write_frame(Frame::binary(Payload::Owned(data))).await
    }

    /// 控制帧 payload 上限 125 字节
    async fn ping(&mut self, data: Option<Vec<u8>>) -> Result<(), String> {
        let payload = data.unwrap_or_default();
        if payload.len() > 125 {
            return Err("ping: payload exceeds 125 bytes".to_string());
        }
        self.write_frame(Frame::new(
            true,
            OpCode::Ping,
            None,
            Payload::Owned(payload),
        ))
        .await
    }

    /// 收一条完整消息（分片已合并）：("text"|"binary", data)；
    /// 对端关闭返回 nil,"closed"；超时返回 nil,"timeout"（连接仍可继续用）
    async fn receive(&mut self) -> Result<WsMessage, String> {
        let mut ws = self.take()?;
        let deadline = self.deadline();
        loop {
            match with_deadline(deadline, ws.read_frame()).await {
                Ok(Ok(frame)) => match frame.opcode {
                    OpCode::Text | OpCode::Binary => {
                        let op = if frame.opcode == OpCode::Text {
                            "text"
                        } else {
                            "binary"
                        };
                        let data = payload_bytes(frame.payload);
                        self.put(ws);
                        return Ok((op, LuaBytes(data)));
                    }
                    // 对端 close：auto_close 已回过 close 帧，连接结束
                    OpCode::Close => return Err("closed".to_string()),
                    // auto_pong 吃掉 ping，这里只可能见到对端的 pong；忽略继续收
                    _ => continue,
                },
                // 读出错（含帧过大、协议错误）：流状态不可信，直接丢弃
                Ok(Err(err)) => return Err(err.to_string()),
                Err(()) => {
                    self.put(ws);
                    return Err("timeout".to_string());
                }
            }
        }
    }

    /// 发 close 帧并结束连接；不给 code 时用 1000
    async fn close(&mut self, code: Option<i64>, reason: Option<Vec<u8>>) -> Result<(), String> {
        let code = match code {
            None => 1000u16,
            Some(c) => u16::try_from(c)
                .ok()
                .filter(|c| (1000..5000).contains(c))
                .ok_or_else(|| format!("close: invalid code {c}"))?,
        };
        let reason = reason.unwrap_or_default();
        self.write_frame(Frame::close(code, &reason)).await?;
        // close 之后写半已关闭：丢弃流，state() 变 1
        self.ws.borrow_mut().take();
        Ok(())
    }
}

/// 服务端：101 握手完成后的帧循环（在独立任务里跑一个 Lua 协程）
pub(crate) async fn run_conn(
    ws: WebSocket<TokioIo<Upgraded>>,
    handler: LuaFunction,
    request: HttpRequest,
    max_message: usize,
) {
    let shared = Rc::new(RefCell::new(Some(new_stream(ws, max_message))));
    let conn = WsConn {
        ws: shared.clone(),
        timeout: None,
    };
    let mut co = unsafe { handler.call_in_new_thread((conn, request)) };
    if let Err(err) = (&mut co).await {
        eprintln!("websocket handler error: {err}");
    }
    drop(co); // 解除协程锚定（挂起中被取消同样适用）
    // handler 返回（或被取消）后由服务端主动 close；对端可能已关，忽略错误
    let pending = shared.borrow_mut().take();
    if let Some(mut ws) = pending {
        let _ = ws.write_frame(Frame::close(1000, b"")).await;
    }
}

/// connect 的 options table，逐字段读取由 derive(FromLua) 生成：
///   headers     额外请求头（可覆盖默认的 Host 等）
///   timeout     毫秒：DNS+TCP+TLS+握手的总上限，同时也是 conn 的初始读/写超时
///   ctx         wss 用的 TlsContext（缺省公有根；ALPN 强制 http/1.1）
///   max_message 单条消息上限，缺省 64 MiB
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
pub(crate) struct WsOptions {
    headers: Headers,
    timeout: Option<i64>,
    ctx: Option<ClientCfg>,
    max_message: Option<i64>,
}

/// 客户端入口：静态方法
///   Ws.connect(url, opts?) → conn
pub struct Ws;

#[cosmos::userdata(name = "network.websocket")]
impl Ws {
    /// ws:// 明文，wss:// 走 ctx（缺省公有根）+ 强制 ALPN http/1.1
    async fn connect(url: String, opts: Option<WsOptions>) -> Result<WsConn, String> {
        let opts = opts.unwrap_or_default();
        let uri: Uri = url
            .parse()
            .map_err(|e| format!("invalid url '{url}': {e}"))?;
        let secure = match uri.scheme_str() {
            Some("ws") => false,
            Some("wss") => true,
            other => {
                return Err(format!(
                    "unsupported scheme '{}' (expected ws or wss)",
                    other.unwrap_or("")
                ))
            }
        };
        let host = uri
            .host()
            .ok_or_else(|| format!("missing host in url '{url}'"))?;
        // Uri::host 对 IPv6 带方括号，getaddrinfo / SNI 都要去掉
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
        let target = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let host_header = if uri.port_u16().is_some() {
            format!("{host}:{port}")
        } else {
            host.to_string()
        };

        let mut builder = Request::builder()
            .method(Method::GET)
            .uri(target)
            .header(HOST, host_header)
            .header(UPGRADE, "websocket")
            .header(CONNECTION, "upgrade")
            .header("Sec-WebSocket-Key", handshake::generate_key())
            .header("Sec-WebSocket-Version", "13");
        for (k, v) in &opts.headers.0 {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| format!("invalid header name '{k}'"))?;
            let value = HeaderValue::from_bytes(v.as_bytes())
                .map_err(|_| format!("invalid header value for '{k}'"))?;
            builder = builder.header(name, value);
        }
        let request = builder
            .body(Empty::<Bytes>::new())
            .map_err(|e| format!("build request: {e}"))?;

        let connect_deadline = opts
            .timeout
            .and_then(timeout_from_ms)
            .map(|d| Instant::now() + d);
        let io = if secure {
            let cfg = match opts.ctx {
                Some(ClientCfg(c)) => c,
                None => default_tls_config(),
            };
            let cfg = with_alpn_client(cfg, &[b"http/1.1"]);
            let tls = with_deadline(
                connect_deadline,
                connect_tls_io(host, port, None, Some(ClientCfg(cfg))),
            )
            .await
            .map_err(|_| "timeout".to_string())??;
            crate::http::Conn::Tls(tls)
        } else {
            let stream = with_deadline(connect_deadline, TcpStream::connect((host, port)))
                .await
                .map_err(|_| "timeout".to_string())?
                .map_err(|e| classify_io(&e))?;
            crate::http::Conn::Plain(stream)
        };

        let (ws, _resp) = with_deadline(
            connect_deadline,
            handshake::client(&TokioExecutor::new(), request, io),
        )
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| format!("websocket handshake: {e}"))?;

        let max_message = opts
            .max_message
            .filter(|n| *n > 0)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_MESSAGE);
        Ok(WsConn {
            ws: Rc::new(RefCell::new(Some(new_stream(ws, max_message)))),
            timeout: opts.timeout.and_then(timeout_from_ms),
        })
    }
}
