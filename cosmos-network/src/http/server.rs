//! HTTP 服务端：hyper auto (h1/h2) 驱动连接，每个请求跑一个 Lua 协程。
//!
//! Lua 侧：
//!   local HttpServer = require("cosmos.network.http.server")
//!   local srv = assert(HttpServer.listen("0.0.0.0", 8080, function(req)
//!     -- req: method()/path()/target()/version()/headers()/header(name)/
//!     --      body()/remote_addr()
//!     return { status = 200, headers = {...}, body = "hello" }
//!     -- 选定 path 升级 WebSocket：return { websocket = function(conn, req) ... end }
//!   end, { tls = server_ctx, max_body = 8 * 1024 * 1024,
//!          ws_max_message = 4 * 1024 * 1024 }))
//!   srv:close()
//!
//! handler 在注册表里锚定（LuaFunction），每个请求由运行时创建独立轻线程执行；
//! 协程之间、以及握手/读写任务之间都在同一个 LocalSet 上交错推进。
//! 连接由 hyper auto builder 处理（h1 keep-alive、h2 多路复用），
//! 默认全量读取请求体；stream_body 模式在头部就绪时调用 handler，按需逐块消费。
//!
//! WebSocket 不单独监听：handler 对选定的 path 返回
//! `{ websocket = function(conn, req) ... end }`，本层把保留的请求 Parts
//! 交给 fastwebsockets 完成 101 升级，之后连接脱离 HTTP（见 websocket/conn.rs）。

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use super::lifecycle::Life;
use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::body::{Body as HyperBody, Frame, SizeHint};
use hyper::header::{HeaderName, HeaderValue};
use hyper::service::Service;
use hyper::{Request as HyperRequest, Response, StatusCode};
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto;
use std::io;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use cosmos::userdata::{LuaFunction, Nil};

use super::body::{integer, Body, BodyCleanup, BodyValue, Outgoing};
use super::{Headers, LuaBytes};
use crate::error::classify_io;
use crate::tcp::tls::{ServerCfg, TlsIo};

const DEFAULT_MAX_BODY: usize = 64 * 1024 * 1024;
const DEFAULT_WS_MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// h2 的每个 stream 由 hyper 交给 executor 跑。Lua 协程是 !Send，
/// 因此这里固定 spawn_local：所有任务留在 LocalSet 的同一条线程上。
/// 不用 TokioExecutor 是因为它的 Executor impl 要求 future 是 Send。
#[derive(Clone)]
struct LocalExec;

impl<F> hyper::rt::Executor<F> for LocalExec
where
    F: Future<Output = ()> + 'static,
{
    fn execute(&self, fut: F) {
        tokio::task::spawn_local(fut);
    }
}

/// listen 的第四个参数（options table），逐字段读取由 derive(FromLua) 生成
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct ServerOptions {
    max_body: Option<i64>,
    tls: Option<ServerCfg>,
    ws_max_message: Option<i64>,
    stream_body: bool,
}

/// 服务端共享状态：锚定的 handler；Rc 由 accept 循环与各连接共同持有
struct ServerInner {
    life: Rc<Life>,
    handler: LuaFunction,
    max_body: usize,
    ws_max_message: usize,
    stream_body: bool,
}

/// 服务端请求（Lua userdata）
#[derive(Clone)]
pub(crate) struct HttpRequest {
    method: String,
    target: String,
    path: String,
    version: i64,
    headers: Headers,
    body: BodyValue,
    remote: String,
}

#[cosmos::userdata(name = "network.http.request")]
impl HttpRequest {
    pub(super) fn method(&self) -> String {
        self.method.clone()
    }
    /// 原始 request-target（含 query）
    fn target(&self) -> String {
        self.target.clone()
    }

    fn path(&self) -> String {
        self.path.clone()
    }

    /// 10 / 11 / 20（h1.0 / h1.1 / h2）
    fn version(&self) -> i64 {
        self.version
    }

    fn headers(&self) -> Headers {
        self.headers.clone()
    }

    pub(super) fn header(&self, name: String) -> Option<String> {
        self.headers.get(&name).map(str::to_string)
    }

    async fn body(self, limit: Option<f64>) -> Result<LuaBytes, String> {
        match self.body {
            BodyValue::Bytes(bytes) => {
                if let Some(limit) = limit {
                    if bytes.0.len() as u64 > integer(limit, 64.0 * 1024.0 * 1024.0, "body limit")?
                    {
                        return Err("HTTP body exceeds collection limit".into());
                    }
                }
                Ok(bytes)
            }
            BodyValue::Stream(body) => {
                let limit = integer(
                    limit.unwrap_or(64.0 * 1024.0 * 1024.0),
                    64.0 * 1024.0 * 1024.0,
                    "body limit",
                )?;
                body.collect(Some(limit)).await.map(LuaBytes)
            }
        }
    }
    fn stream(&self) -> Body {
        match &self.body {
            BodyValue::Bytes(bytes) => Body::bytes(bytes.0.clone()),
            BodyValue::Stream(body) => body.clone(),
        }
    }

    fn remote_addr(&self) -> String {
        self.remote.clone()
    }
}

impl HttpRequest {
    fn from_parts(parts: &hyper::http::request::Parts, body: BodyValue, peer: &str) -> Self {
        HttpRequest {
            method: parts.method.as_str().to_string(),
            target: parts
                .uri
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/")
                .to_string(),
            path: parts.uri.path().to_string(),
            version: super::version_num(parts.version),
            headers: Headers(
                parts
                    .headers
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_string(),
                            String::from_utf8_lossy(v.as_bytes()).into_owned(),
                        )
                    })
                    .collect(),
            ),
            body,
            remote: peer.to_string(),
        }
    }
}

/// handler 的返回值：table { status, headers, body, websocket }，
/// 缺省按 `RespSpec::default()`（200 + 空头 + 空 body）取值；
/// 给了 websocket 函数则改走 101 升级（忽略 status/headers/body）
#[derive(cosmos::FromLua)]
#[lua(default)]
struct RespSpec {
    status: u16,
    headers: Headers,
    body: BodyValue,
    websocket: Option<LuaFunction>,
}

/// Lua handler 的返回形态由宏读取；HTTP 层只处理转换后的值。
#[derive(cosmos::FromLua)]
enum HandlerReply {
    #[lua(type = "table")]
    Response(RespSpec),
    #[lua(type = "string")]
    Body(LuaBytes),
    #[lua(type = "nil")]
    Empty(Nil, ReplyError),
    #[lua(type = "none")]
    Nothing,
}

/// 保持 nil 返回的原有约定：第二个值只有是字符串时才表示错误。
#[derive(cosmos::FromLua)]
enum ReplyError {
    #[lua(type = "string")]
    Message(String),
    #[lua(other)]
    Absent,
}

impl Default for RespSpec {
    fn default() -> Self {
        RespSpec {
            status: 200,
            headers: Headers::default(),
            body: BodyValue::default(),
            websocket: None,
        }
    }
}

/// 服务端保留请求输入到响应完成，允许请求流直接转发或由输出回调延迟读取。
struct ServerBody {
    outgoing: Outgoing,
    input: Option<Body>,
}
impl Drop for ServerBody {
    fn drop(&mut self) {
        if let Some(input) = &self.input {
            input.stop();
        }
    }
}
impl HyperBody for ServerBody {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        Pin::new(&mut self.get_mut().outgoing).poll_frame(cx)
    }
    fn size_hint(&self) -> SizeHint {
        self.outgoing.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.outgoing.is_end_stream()
    }
}
struct RequestCleanup(Option<Body>);
impl Drop for RequestCleanup {
    fn drop(&mut self) {
        if let Some(body) = &self.0 {
            body.stop();
        }
    }
}
impl RespSpec {
    fn into_response(
        self,
        head: bool,
        input: Option<Body>,
    ) -> Result<Response<ServerBody>, String> {
        let mut input = RequestCleanup(input);
        let mut source = BodyCleanup(Some(self.body.clone()));
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let no_body = status.is_informational() || matches!(status.as_u16(), 204 | 205 | 304);
        let known = self.body.length();
        let mut builder = Response::builder().status(status);
        for (k, v) in self.headers.0 {
            if k == "transfer-encoding" {
                continue;
            }
            if k == "content-length" {
                if no_body {
                    continue;
                }
                let supplied = v
                    .parse::<u64>()
                    .map_err(|_| "invalid response content-length")?;
                if !head && Some(supplied) != known {
                    if matches!(&self.body, BodyValue::Stream(_)) {
                        return Err("content-length must match a body with known length".into());
                    }
                    continue;
                }
            }
            let name =
                HeaderName::from_bytes(k.as_bytes()).map_err(|_| "invalid response header name")?;
            let value = HeaderValue::from_bytes(v.as_bytes())
                .map_err(|_| "invalid response header value")?;
            builder = builder.header(name, value);
        }
        let outgoing = if head || no_body {
            self.body.cancel();
            if head
                && !no_body
                && !builder
                    .headers_ref()
                    .is_some_and(|h| h.contains_key("content-length"))
            {
                if let Some(length) = known {
                    builder = builder.header("content-length", length);
                }
            }
            Outgoing::empty()
        } else {
            self.body.into_wire()?
        };
        let response = builder
            .body(ServerBody {
                outgoing,
                input: input.0.take(),
            })
            .map_err(|e| format!("build response: {e}"))?;
        source.0.take();
        Ok(response)
    }
}
fn plain_response(status: StatusCode, msg: &str) -> Response<ServerBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(ServerBody {
            outgoing: BodyValue::Bytes(LuaBytes(Bytes::copy_from_slice(msg.as_bytes())))
                .into_wire()
                .expect("byte body"),
            input: None,
        })
        .expect("static headers")
}

/// 每个连接一个 service 实例；请求处理完把结果收敛成 hyper Response
struct LuaService {
    inner: Rc<ServerInner>,
    peer: Arc<str>,
}

impl Service<HyperRequest<Incoming>> for LuaService {
    type Response = Response<ServerBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>>>>;

    fn call(&self, req: HyperRequest<Incoming>) -> Self::Future {
        let inner = self.inner.clone();
        let peer = self.peer.clone();
        let guard = inner.life.request();
        let mut force = inner.life.forcing();
        Box::pin(async move {
            let _guard = guard;
            if *force.borrow() {
                return Ok(plain_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "server shutting down",
                ));
            }
            tokio::select! {biased;
                _=force.changed()=>Ok(plain_response(StatusCode::SERVICE_UNAVAILABLE,"server shutting down")),
                response=handle(req,inner,peer)=>Ok(response),
            }
        })
    }
}

async fn handle(
    req: HyperRequest<Incoming>,
    inner: Rc<ServerInner>,
    peer: Arc<str>,
) -> Response<ServerBody> {
    let (parts, incoming) = req.into_parts();
    let (body, mut cleanup) = if inner.stream_body {
        let body = Body::incoming(incoming, None, Some(inner.max_body as u64), None);
        (BodyValue::Stream(body.clone()), RequestCleanup(Some(body)))
    } else {
        let body = match Limited::new(incoming, inner.max_body).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => return plain_response(StatusCode::PAYLOAD_TOO_LARGE, "payload too large"),
        };
        (BodyValue::Bytes(LuaBytes(body)), RequestCleanup(None))
    };
    let request = HttpRequest::from_parts(&parts, body.clone(), &peer);
    let result = match run_handler(&inner, request).await {
        Ok(mut spec) => match spec.websocket.take() {
            Some(handler) => {
                let body = match body {
                    BodyValue::Bytes(bytes) => Ok(bytes),
                    BodyValue::Stream(body) => body
                        .collect(Some(inner.max_body as u64))
                        .await
                        .map(LuaBytes),
                };
                match body {
                    Ok(body) => {
                        let request =
                            HttpRequest::from_parts(&parts, BodyValue::Bytes(body), &peer);
                        return upgrade_websocket(parts, handler, request, inner.ws_max_message);
                    }
                    Err(error) => Err(error),
                }
            }
            None => spec.into_response(parts.method == hyper::Method::HEAD, cleanup.0.take()),
        },
        Err(error) => Err(error),
    };
    match result {
        Ok(response) => response,
        Err(error) => {
            eprintln!("http handler error: {error}");
            plain_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
        }
    }
}

/// 把保留的请求 Parts 交给 fastwebsockets 完成 101 升级，返回握手响应；
/// 升级完成后的帧循环在独立任务里跑 Lua 协程（见 websocket::conn::run_conn）
fn upgrade_websocket(
    parts: hyper::http::request::Parts,
    handler: LuaFunction,
    request: HttpRequest,
    ws_max_message: usize,
) -> Response<ServerBody> {
    use fastwebsockets::upgrade;

    let mut req = HyperRequest::from_parts(parts, ());
    if !upgrade::is_upgrade_request(&req) {
        return plain_response(
            StatusCode::BAD_REQUEST,
            "expected a websocket upgrade request",
        );
    }
    let (resp, fut) = match upgrade::upgrade(&mut req) {
        Ok(v) => v,
        Err(err) => {
            return plain_response(
                StatusCode::BAD_REQUEST,
                &format!("websocket upgrade: {err}"),
            )
        }
    };
    tokio::task::spawn_local(async move {
        match fut.await {
            Ok(ws) => crate::websocket::conn::run_conn(ws, handler, request, ws_max_message).await,
            Err(err) => eprintln!("websocket upgrade error: {err}"),
        }
    });
    let (parts, _) = resp.into_parts();
    Response::from_parts(
        parts,
        ServerBody {
            outgoing: Outgoing::empty(),
            input: None,
        },
    )
}

/// 类型化回调负责协程生命周期与结果转换，本层只组装 HTTP 响应。
async fn run_handler(inner: &Rc<ServerInner>, request: HttpRequest) -> Result<RespSpec, String> {
    match inner.handler.call::<_, HandlerReply>(request).await? {
        HandlerReply::Nothing | HandlerReply::Empty(_, ReplyError::Absent) => {
            Ok(RespSpec::default())
        }
        HandlerReply::Response(spec) => Ok(spec),
        // 直接返回字符串：200 + 原文
        HandlerReply::Body(body) => Ok(RespSpec {
            status: 200,
            headers: Headers::default(),
            body: BodyValue::Bytes(body),
            websocket: None,
        }),
        HandlerReply::Empty(_, ReplyError::Message(err)) => Err(err),
    }
}

async fn accept_loop(inner: Rc<ServerInner>, listener: TcpListener, acceptor: Option<TlsAcceptor>) {
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("http server accept error: {e}");
                continue;
            }
        };
        let inner = inner.clone();
        let acceptor = acceptor.clone();
        let peer: Arc<str> = peer.to_string().into();
        let (id, guard) = inner.life.connection();
        let life = inner.life.clone();
        let mut draining = life.draining();
        let task = tokio::task::spawn_local(async move {
            let _guard = guard;
            if *draining.borrow() {
                return;
            }
            match acceptor {
                None => serve_conn(TokioIo::new(sock), inner, peer).await,
                Some(acceptor) => match tokio::select! {biased;
                    _=draining.changed()=>return,
                    result=acceptor.accept(crate::tcp::transport::PlainIo::tcp(sock))=>result,
                } {
                    Ok(stream) => {
                        serve_conn(TokioIo::new(TlsIo::Server(stream)), inner, peer).await
                    }
                    Err(e) => eprintln!("http tls handshake error: {}", classify_io(&e)),
                },
            }
        });
        life.track(id, task.abort_handle());
    }
}

async fn serve_conn<I>(io: I, inner: Rc<ServerInner>, peer: Arc<str>)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let mut draining = inner.life.draining();
    let service = LuaService { inner, peer };
    let builder = auto::Builder::new(LocalExec);
    // 客户端断开/keep-alive 超时等连接级错误不必上报；handler 错误已单独记日志。
    // with_upgrades：HTTP 请求可被 handler 升级成 WebSocket（101 后连接脱管）
    let connection = builder.serve_connection_with_upgrades(io, service);
    tokio::pin!(connection);
    if !*draining.borrow() {
        tokio::select! {biased;_=draining.changed()=>{},_=connection.as_mut()=>return}
    }
    connection.as_mut().graceful_shutdown();
    let _ = connection.await;
}

/// 服务端句柄：close() 停止 accept（已有连接继续跑到结束）。
/// 与 C++ 一致：句柄被 GC 回收不等于 close（accept 任务持有自己的 Rc），
/// 只有显式 close() 或进程退出才停止监听
#[derive(Clone)]
pub(crate) struct HttpServerHandle {
    life: Rc<Life>,
    port: u16,
}

#[cosmos::userdata(name = "network.http.handle")]
impl HttpServerHandle {
    fn close(&self) {
        self.life.close();
    }
    async fn shutdown(self, timeout: Option<Duration>) -> Result<bool, String> {
        self.life.start_drain();
        if let Some(timeout) = timeout {
            if tokio::time::timeout(timeout, self.life.wait())
                .await
                .is_err()
            {
                self.life.force();
                self.life.wait().await;
                return Ok(false);
            }
        } else {
            self.life.wait().await;
        }
        Ok(true)
    }
    fn is_draining(&self) -> bool {
        *self.life.draining().borrow()
    }
    fn connections(&self) -> i64 {
        self.life.connections() as i64
    }

    /// 实际监听端口（传 0 让系统分配时用）
    fn port(&self) -> i64 {
        self.port as i64
    }
}

/// 服务端入口：静态方法
///   HttpServer.listen(host, port, handler, opts?) → handle
pub(crate) struct HttpServer;

#[cosmos::userdata(name = "network.http.server")]
impl HttpServer {
    /// opts 里给 tls = server_ctx 即启用 HTTPS（证书/私钥在该 ctx 上设置）
    async fn listen(
        host: String,
        port: i64,
        handler: LuaFunction,
        opts: Option<ServerOptions>,
    ) -> Result<HttpServerHandle, String> {
        let opts = opts.unwrap_or_default();
        if !(0..=65535).contains(&port) {
            return Err("HTTP server port must be in 0..=65535".into());
        }
        let listener = TcpListener::bind((host.as_str(), port as u16))
            .await
            .map_err(|e| classify_io(&e))?;
        let local_port = listener.local_addr().map_err(|e| classify_io(&e))?.port();
        let life = Life::new();
        let inner = Rc::new(ServerInner {
            life: life.clone(),
            handler,
            stream_body: opts.stream_body,
            max_body: opts
                .max_body
                .filter(|n| *n > 0)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_BODY),
            ws_max_message: opts
                .ws_max_message
                .filter(|n| *n > 0)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_WS_MAX_MESSAGE),
        });
        let acceptor = opts.tls.map(|c| TlsAcceptor::from(c.0));
        let guard = life.accept_guard();
        let task = tokio::task::spawn_local(async move {
            let _guard = guard;
            accept_loop(inner, listener, acceptor).await;
        });
        life.install(task);
        Ok(HttpServerHandle {
            life,
            port: local_port,
        })
    }
}
