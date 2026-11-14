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
//! 后台读任务持续完整收帧，独立写半串行化消息与 pong/close；同一连接可并发收发。

pub(crate) mod conn;

use bytes::Bytes;
use fastwebsockets::handshake;
use http_body_util::Empty;
use hyper::header::{HeaderName, HeaderValue, CONNECTION, HOST, UPGRADE};
use hyper::{Method, Request, Uri};
use hyper_util::rt::TokioExecutor;
use tokio::time::Instant;

use self::conn::WsConn;
use crate::deadline::{timeout_from_ms, with_deadline};
use crate::http::client::default_tls_config;
use crate::http::Headers;
use crate::tcp::tls::{with_alpn_client, ClientCfg};

const DEFAULT_MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// connect 的 options table，逐字段读取由 derive(FromLua) 生成：
///   headers     额外请求头（可覆盖默认的 Host 等）
///   timeout     毫秒：DNS+TCP+TLS+握手的总上限，同时也是 conn 的初始读/写超时
///   ctx         wss 用的 TlsContext（缺省公有根；ALPN 强制 http/1.1）
///   max_message 单条消息上限，缺省 64 MiB
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct WsOptions {
    headers: Headers,
    timeout: Option<i64>,
    ctx: Option<ClientCfg>,
    max_message: Option<i64>,
    proxy: Option<crate::proxy::Proxy>,
}

/// 客户端入口：静态方法
///   Ws.connect(url, opts?) → conn
pub(crate) struct Ws;

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
        let config = if secure {
            let cfg = opts.ctx.map_or_else(default_tls_config, |ctx| ctx.0);
            Some(with_alpn_client(cfg, &[b"http/1.1"]))
        } else {
            None
        };
        let io = with_deadline(
            connect_deadline,
            crate::proxy::connect(host, port, config, opts.proxy.as_ref(), true),
        )
        .await
        .map_err(|_| "timeout".to_owned())??;

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
        Ok(WsConn::new(
            ws,
            max_message,
            opts.timeout.and_then(timeout_from_ms),
        ))
    }
}
