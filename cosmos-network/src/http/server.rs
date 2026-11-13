//! HTTP 服务端：hyper auto (h1/h2) 驱动连接，每个请求跑一个 Lua 协程。
//!
//! Lua 侧：
//!   local HttpServer = require("cosmos.HttpServer")
//!   local srv = assert(HttpServer.listen("0.0.0.0", 8080, function(req)
//!     -- req: method()/path()/target()/version()/headers()/header(name)/
//!     --      body()/remote_addr()
//!     return { status = 200, headers = {...}, body = "hello" }
//!   end, { tls = server_ctx, max_body = 8 * 1024 * 1024 }))
//!   srv:close()
//!
//! handler 在注册表里锚定（LuaFunction），每个请求新建 lua_State 协程执行；
//! 协程之间、以及握手/读写任务之间都在同一个 LocalSet 上交错推进。
//! 连接由 hyper auto builder 处理（h1 keep-alive、h2 多路复用），
//! 请求体在 v1 全量读入内存（max_body 封顶）。

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

use cosmos::coroutine::LuaCoroutine;
use cosmos::ffi::*;
use cosmos::userdata::{push, FromLua, LuaFunction};

use super::{take_bytes, take_string, Headers, LuaBytes};
use crate::error::classify_io;
use crate::tls::{ServerCfg, TlsIo};

const DEFAULT_MAX_BODY: usize = 64 * 1024 * 1024;

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

/// listen 的第四个参数（options table）
#[derive(Default)]
pub(crate) struct ServerOptions {
    max_body: Option<i64>,
    tls: Option<ServerCfg>,
}

impl FromLua for ServerOptions {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        let mut opts = ServerOptions::default();
        if lua_type(L, idx) != LUA_TTABLE {
            return Err(format!("argument {idx}: expected options table"));
        }
        lua_pushnil(L);
        while lua_next(L, idx) != 0 {
            if lua_type(L, -2) == LUA_TSTRING && lua_type(L, -1) != LUA_TNIL {
                let key = take_string(L, -2);
                let top = lua_gettop(L);
                match key.as_str() {
                    "max_body" => opts.max_body = Some(lua_tointeger(L, top) as i64),
                    "tls" => opts.tls = Some(ServerCfg::from_lua(L, top)?),
                    _ => {}
                }
            }
            lua_settop(L, -2);
        }
        Ok(opts)
    }
}

/// 服务端共享状态：主状态 + 锚定的 handler；Rc 由 accept 循环与各连接共同持有
struct ServerInner {
    main: *mut lua_State,
    handler: LuaFunction,
    max_body: usize,
}

/// 服务端请求（Lua userdata）
pub struct HttpRequest {
    method: String,
    target: String,
    path: String,
    version: i64,
    headers: Headers,
    body: Bytes,
    remote: String,
}

#[cosmos::userdata]
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

/// handler 的返回值：table { status, headers, body }
pub(crate) struct RespSpec {
    status: u16,
    headers: Headers,
    body: Vec<u8>,
}

impl Default for RespSpec {
    fn default() -> Self {
        RespSpec {
            status: 200,
            headers: Headers(Vec::new()),
            body: Vec::new(),
        }
    }
}

impl FromLua for RespSpec {
    unsafe fn from_lua(L: *mut lua_State, idx: c_int) -> Result<Self, String> {
        let mut spec = RespSpec::default();
        lua_pushnil(L);
        while lua_next(L, idx) != 0 {
            if lua_type(L, -2) == LUA_TSTRING && lua_type(L, -1) != LUA_TNIL {
                let key = take_string(L, -2);
                let top = lua_gettop(L);
                match key.as_str() {
                    "status" => {
                        let s = lua_tointeger(L, top) as i64;
                        if !(100..=999).contains(&s) {
                            return Err(format!("invalid response status {s}"));
                        }
                        spec.status = s as u16;
                    }
                    "headers" => spec.headers = Headers::from_lua(L, top)?,
                    "body" => spec.body = take_bytes(L, top),
                    _ => {}
                }
            }
            lua_settop(L, -2);
        }
        Ok(spec)
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
                return plain_response(StatusCode::INTERNAL_SERVER_ERROR, "invalid response header");
            };
            builder = builder.header(name, value);
        }
        builder
            .body(Full::new(Bytes::from(self.body)))
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
    let request = HttpRequest {
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
    };
    match run_handler(&inner, request).await {
        Ok(spec) => spec.into_response(),
        Err(err) => {
            eprintln!("http handler error: {err}");
            plain_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
        }
    }
}

/// 新建 lua_State 协程执行 handler(req)；完成后从协程栈读返回值
async fn run_handler(inner: &Rc<ServerInner>, request: HttpRequest) -> Result<RespSpec, String> {
    let main = inner.main;
    let mut co = unsafe {
        let thread = lua_newthread(main); // 线程对象压到 main 栈上
        lua_rawgeti(thread, LUA_REGISTRYINDEX, inner.handler.registry_ref());
        push(thread, request);
        // LuaCoroutine::new 里 luaL_ref(main) 会把线程对象从 main 栈上取走
        LuaCoroutine::new(main, thread)
    };
    let result = (&mut co).await;
    let out = unsafe {
        match result {
            Ok(()) => read_handler_return(co.state()),
            Err(err) => Err(err),
        }
    };
    drop(co); // 解除协程锚定（挂起中被取消同样适用）
    out
}

unsafe fn read_handler_return(L: *mut lua_State) -> Result<RespSpec, String> {
    let top = lua_gettop(L);
    if top == 0 {
        return Ok(RespSpec::default());
    }
    match lua_type(L, 1) {
        LUA_TTABLE => RespSpec::from_lua(L, 1),
        // 直接返回字符串：200 + 原文
        LUA_TSTRING => Ok(RespSpec {
            status: 200,
            headers: Headers(Vec::new()),
            body: take_bytes(L, 1),
        }),
        // handler 里 assert 失败等会返回 nil, err
        LUA_TNIL => {
            if top >= 2 && lua_type(L, 2) == LUA_TSTRING {
                Err(take_string(L, 2))
            } else {
                Ok(RespSpec::default())
            }
        }
        other => Err(format!(
            "handler must return a response table (got lua type {other})"
        )),
    }
}

async fn accept_loop(
    inner: Rc<ServerInner>,
    listener: TcpListener,
    acceptor: Option<TlsAcceptor>,
) {
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
                Some(acceptor) => match acceptor.accept(sock).await {
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
    I: hyper::rt::Read + hyper::rt::Write + Unpin + 'static,
{
    let service = LuaService { inner, peer };
    let builder = auto::Builder::new(LocalExec);
    // 客户端断开/keep-alive 超时等连接级错误不必上报；handler 错误已单独记日志
    let _ = builder.serve_connection(io, service).await;
}

/// 服务端句柄：close() 停止 accept（已有连接继续跑到结束）。
/// 与 C++ 一致：句柄被 GC 回收不等于 close（accept 任务持有自己的 Rc），
/// 只有显式 close() 或进程退出才停止监听
pub struct HttpServerHandle {
    task: Option<JoinHandle<()>>,
    port: u16,
}

#[cosmos::userdata]
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
pub struct HttpServer;

#[cosmos::userdata]
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
        let local_port = listener
            .local_addr()
            .map_err(|e| classify_io(&e))?
            .port();
        let inner = Rc::new(ServerInner {
            main: handler.main(),
            handler,
            max_body: opts
                .max_body
                .filter(|n| *n > 0)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_BODY),
        });
        let acceptor = opts.tls.map(|c| TlsAcceptor::from(c.0));
        let task = tokio::task::spawn_local(accept_loop(inner, listener, acceptor));
        Ok(HttpServerHandle {
            task: Some(task),
            port: local_port,
        })
    }
}

pub unsafe fn register(L: *mut lua_State) {
    cosmos::userdata::register::<HttpServer>(L);
    cosmos::userdata::register::<HttpServerHandle>(L);
    cosmos::userdata::register::<HttpRequest>(L);
}
