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
//! 请求体在 v1 全量读入内存（max_body 封顶）。
//!
//! WebSocket 不单独监听：handler 对选定的 path 返回
//! `{ websocket = function(conn, req) ... end }`，本层把保留的请求 Parts
//! 交给 fastwebsockets 完成 101 升级，之后连接脱离 HTTP（见 websocket/conn.rs）。

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::service::Service;
use hyper::{Request as HyperRequest, Response, StatusCode};
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use cosmos::userdata::{LuaFunction, Nil};

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
}

/// 服务端共享状态：锚定的 handler；Rc 由 accept 循环与各连接共同持有
struct ServerInner {
    handler: LuaFunction,
    max_body: usize,
    ws_max_message: usize,
}

/// 服务端请求（Lua userdata）
pub(crate) struct HttpRequest {
    method: String,
    target: String,
    path: String,
    version: i64,
    headers: Headers,
    body: Bytes,
    remote: String,
}

#[cosmos::userdata(name = "network.http.request")]
impl HttpRequest {
    fn method(&self) -> String {
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

    fn header(&self, name: String) -> Option<String> {
        self.headers.get(&name).map(str::to_string)
    }

    fn body(&self) -> LuaBytes {
        LuaBytes(self.body.clone())
    }

    fn remote_addr(&self) -> String {
        self.remote.clone()
    }
}

impl HttpRequest {
    fn from_parts(parts: &hyper::http::request::Parts, body: Bytes, peer: &str) -> Self {
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
    body: LuaBytes,
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
            body: LuaBytes::default(),
            websocket: None,
        }
    }
}

impl RespSpec {
    fn into_response(self) -> Response<Full<Bytes>> {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut builder = Response::builder().status(status);
        for (k, v) in self.headers.0 {
            // 帧长度交给 hyper 按 body 计算，避免用户填错导致 framing 不一致
            if k == "content-length" || k == "transfer-encoding" {
                continue;
            }
            let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_bytes(v.as_bytes()),
            ) else {
                return plain_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invalid response header",
                );
            };
            builder = builder.header(name, value);
        }
        builder
            .body(Full::new(self.body.0))
            .unwrap_or_else(|_| plain_response(StatusCode::INTERNAL_SERVER_ERROR, "bad response"))
    }
}

fn plain_response(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(msg.to_string())));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// 每个连接一个 service 实例；请求处理完把结果收敛成 hyper Response
struct LuaService {
    inner: Rc<ServerInner>,
    peer: Arc<str>,
}

impl Service<HyperRequest<Incoming>> for LuaService {
    type Response = Response<Full<Bytes>>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>>>>;

    fn call(&self, req: HyperRequest<Incoming>) -> Self::Future {
        let inner = self.inner.clone();
        let peer = self.peer.clone();
        Box::pin(async move { Ok(handle(req, inner, peer).await) })
    }
}

async fn handle(
    req: HyperRequest<Incoming>,
    inner: Rc<ServerInner>,
    peer: Arc<str>,
) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();
    let body = match Limited::new(body, inner.max_body).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return plain_response(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
        }
    };
    // Bytes 克隆只是引用计数：websocket 分支要重建一份请求 userdata 时零拷贝
    let request = HttpRequest::from_parts(&parts, body.clone(), &peer);
    match run_handler(&inner, request).await {
        Ok(mut spec) => match spec.websocket.take() {
            Some(handler) => {
                let request = HttpRequest::from_parts(&parts, body, &peer);
                upgrade_websocket(parts, handler, request, inner.ws_max_message)
            }
            None => spec.into_response(),
        },
        Err(err) => {
            eprintln!("http handler error: {err}");
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
) -> Response<Full<Bytes>> {
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
    Response::from_parts(parts, Full::new(Bytes::new()))
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
            body,
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
        tokio::task::spawn_local(async move {
            match acceptor {
                None => serve_conn(TokioIo::new(sock), inner, peer).await,
                Some(acceptor) => match acceptor
                    .accept(crate::tcp::transport::PlainIo::tcp(sock))
                    .await
                {
                    Ok(stream) => {
                        serve_conn(TokioIo::new(TlsIo::Server(stream)), inner, peer).await
                    }
                    Err(e) => eprintln!("http tls handshake error: {}", classify_io(&e)),
                },
            }
        });
    }
}

async fn serve_conn<I>(io: I, inner: Rc<ServerInner>, peer: Arc<str>)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let service = LuaService { inner, peer };
    let builder = auto::Builder::new(LocalExec);
    // 客户端断开/keep-alive 超时等连接级错误不必上报；handler 错误已单独记日志。
    // with_upgrades：HTTP 请求可被 handler 升级成 WebSocket（101 后连接脱管）
    let _ = builder.serve_connection_with_upgrades(io, service).await;
}

/// 服务端句柄：close() 停止 accept（已有连接继续跑到结束）。
/// 与 C++ 一致：句柄被 GC 回收不等于 close（accept 任务持有自己的 Rc），
/// 只有显式 close() 或进程退出才停止监听
pub(crate) struct HttpServerHandle {
    task: Option<JoinHandle<()>>,
    port: u16,
}

#[cosmos::userdata(name = "network.http.handle")]
impl HttpServerHandle {
    fn close(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
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
        let listener = TcpListener::bind((host.as_str(), port as u16))
            .await
            .map_err(|e| classify_io(&e))?;
        let local_port = listener.local_addr().map_err(|e| classify_io(&e))?.port();
        let inner = Rc::new(ServerInner {
            handler,
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
        let task = tokio::task::spawn_local(accept_loop(inner, listener, acceptor));
        Ok(HttpServerHandle {
            task: Some(task),
            port: local_port,
        })
    }
}
