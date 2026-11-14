//! HTTP 客户端：hyper + hyper-util 连接池，外挂重定向与整体超时。
//!
//! Lua 侧：
//!   local HttpClient = require("cosmos.network.http.client")
//!   local client = HttpClient.new({ ctx = tls_ctx }) -- ctx 可选，创建时固定 TLS 配置
//!   local resp, err = client:request("GET", "http://host/path", {
//!     headers = { ["x-a"] = "1" },
//!     body = "...",          -- 任意方法都可带 body
//!     timeout = 3000,        -- 毫秒，覆盖连接+重定向+读完 body
//!     redirects = 5,         -- 最多跟随几次重定向，0 关闭
//!     max_body = 1048576,    -- 可选：响应体上限，超过返回错误
//!   })
//!   -- resp = { status=200, version=11, headers={...}, body="..." }
//!
//! 每个 client 独立持有连接池，克隆句柄和并发请求共享该实例的池。
//! 默认（不传 ctx）使用 webpki 公有根 + ALPN h2/http1.1。
//! v1 不处理 gzip（固定发 Accept-Encoding: identity）。

use crate::proxy::{Proxy, Transport};
use cosmos::userdata::{LuaFunction, Nil};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use hyper::header::{HeaderName, HeaderValue, ACCEPT_ENCODING};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::{ClientConfig, RootCertStore};
use tokio::sync::watch;
use tokio::time::Instant;

use super::body::{Body, BodyCleanup, BodyValue, Outgoing};
use super::{version_num, Headers, LuaBytes};
use crate::deadline::{timeout_from_ms, with_deadline};
use crate::tcp::tls::ClientCfg;

type BoxErr = Box<dyn StdError + Send + Sync>;
type HttpPool = Client<Connector, Outgoing>;

fn default_alpn() -> Vec<Vec<u8>> {
    vec![b"h2".to_vec(), b"http/1.1".to_vec()]
}

/// 默认 HTTP 客户端 TLS 配置：公有根 + h2/http1.1 ALPN（只构建一次）
static DEFAULT_TLS: OnceLock<Arc<ClientConfig>> = OnceLock::new();

pub(crate) fn default_tls_config() -> Arc<ClientConfig> {
    DEFAULT_TLS
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let mut cfg = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            cfg.alpn_protocols = default_alpn();
            Arc::new(cfg)
        })
        .clone()
}

/// 路由在 LocalSet 选定；connector 仅持有原生配置和生命周期通知。
#[derive(Clone)]
struct Connector {
    tls: Arc<ClientConfig>,
    proxy: Option<Proxy>,
    shutdown: watch::Receiver<bool>,
    timeout: Option<Duration>,
}
impl tower_service::Service<Uri> for Connector {
    type Response = TokioIo<Transport>;
    type Error = BoxErr;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, uri: Uri) -> Self::Future {
        let tls = self.tls.clone();
        let proxy = self.proxy.clone();
        let mut shutdown = self.shutdown.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            let host = uri
                .host()
                .ok_or_else(|| -> BoxErr { "missing host in request URI".into() })?;
            let host = host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host);
            let secure = uri.scheme_str() == Some("https");
            let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
            let deadline = timeout.map(|d| Instant::now() + d);
            tokio::select! {
                biased;
                _=shutdown.changed()=>Err("HTTP client closed during connection".into()),
                result=with_deadline(deadline,crate::proxy::connect(host,port,secure.then_some(tls),proxy.as_ref(),false))=>{
                    result.map_err(|_|->BoxErr{"connection timeout".into()})?.map(TokioIo::new).map_err(|e|->BoxErr{e.into()})
                }
            }
        })
    }
}
#[derive(cosmos::FromLua)]
enum Route {
    #[lua(type = "userdata")]
    Fixed(Proxy),
    #[lua(type = "function")]
    Select(LuaFunction),
}
#[derive(cosmos::FromLua)]
enum RouteReply {
    #[lua(type = "userdata")]
    Proxy(Proxy),
    #[lua(type = "nil")]
    Direct(Nil, Option<String>),
    #[lua(type = "none")]
    Nothing,
}

/// 构造时固定的 TLS 配置，由该 client 的全部请求与重定向共用。
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct ClientOptions {
    ctx: Option<ClientCfg>,
    proxy: Option<Route>,
    connect_timeout: Option<i64>,
}

/// request 的 options table。
/// 表的逐字段读取由 `#[derive(FromLua)]` 生成（见 cosmos-macros）；
/// `#[lua(default)]` 表示整张表缺省时按 `Default` 取同名字段。
#[derive(cosmos::FromLua)]
#[lua(default)]
struct RequestOptions {
    headers: Headers,
    body: BodyValue,
    timeout: Option<i64>,
    // 显式拒绝请求级 ctx，避免旧调用把 TLS 设置误当成已生效。
    ctx: Option<()>,
    proxy: Option<()>,
    redirects: i64,
    max_body: Option<i64>,
    stream: bool,
}

impl Default for RequestOptions {
    fn default() -> Self {
        RequestOptions {
            headers: Headers::default(),
            body: BodyValue::default(),
            timeout: None,
            ctx: None,
            proxy: None,
            redirects: 5,
            max_body: None,
            stream: false,
        }
    }
}

/// 客户端响应（Lua 侧 table），压栈由 `#[derive(PushLua)]` 生成
#[derive(cosmos::PushLua)]
struct ClientResponse {
    status: u16,
    version: i64,
    headers: Headers,
    body: BodyValue,
}

/// 同一 client 的每条代理路由各有池；回调和缓存只在本地访问。
struct ClientState {
    tls: Arc<ClientConfig>,
    direct: HttpPool,
    routes: RefCell<VecDeque<(Proxy, HttpPool)>>,
    route: Option<Route>,
    timeout: Option<Duration>,
    shutdown: watch::Sender<bool>,
}
#[derive(Clone)]
pub(crate) struct HttpClient(Rc<ClientState>);
fn pool(
    tls: Arc<ClientConfig>,
    proxy: Option<Proxy>,
    shutdown: watch::Receiver<bool>,
    timeout: Option<Duration>,
) -> HttpPool {
    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_timer(TokioTimer::new())
        .pool_max_idle_per_host(32)
        .build(Connector {
            tls,
            proxy,
            shutdown,
            timeout,
        })
}
impl HttpClient {
    async fn route(&self, target: &str) -> Result<Option<Proxy>, String> {
        match &self.0.route {
            None => Ok(None),
            Some(Route::Fixed(proxy)) => Ok(Some(proxy.clone())),
            Some(Route::Select(callback)) => match callback.call(target).await? {
                RouteReply::Proxy(proxy) => Ok(Some(proxy)),
                RouteReply::Direct(_, Some(err)) => Err(err),
                _ => Ok(None),
            },
        }
    }
    fn pool(&self, proxy: Option<&Proxy>) -> HttpPool {
        let Some(proxy) = proxy else {
            return self.0.direct.clone();
        };
        let mut routes = self.0.routes.borrow_mut();
        if let Some(index) = routes.iter().position(|(key, _)| key.same(proxy)) {
            let entry = routes.remove(index).unwrap();
            let pool = entry.1.clone();
            routes.push_back(entry);
            return pool;
        }
        let pool = pool(
            self.0.tls.clone(),
            Some(proxy.clone()),
            self.0.shutdown.subscribe(),
            self.0.timeout,
        );
        if routes.len() == 32 {
            routes.pop_front();
        }
        routes.push_back((proxy.clone(), pool.clone()));
        pool
    }
}
#[cosmos::userdata(name = "network.http.client")]
impl HttpClient {
    fn new(opts: Option<ClientOptions>) -> Self {
        let opts = opts.unwrap_or_default();
        let cfg = match opts.ctx {
            None => default_tls_config(),
            Some(ClientCfg(cfg)) if cfg.alpn_protocols.is_empty() => {
                let mut cfg = (*cfg).clone();
                cfg.alpn_protocols = default_alpn();
                Arc::new(cfg)
            }
            Some(ClientCfg(cfg)) => cfg,
        };
        let timeout = timeout_from_ms(opts.connect_timeout.unwrap_or(10_000));
        let (shutdown, receiver) = watch::channel(false);
        Self(Rc::new(ClientState {
            direct: pool(cfg.clone(), None, receiver, timeout),
            tls: cfg,
            routes: RefCell::new(VecDeque::new()),
            route: opts.proxy,
            timeout,
            shutdown,
        }))
    }

    /// 发一次请求（含重定向跟随）；失败返回 nil + 错误串
    async fn request(
        self,
        method: String,
        url: String,
        opts: Option<RequestOptions>,
    ) -> Result<ClientResponse, String> {
        let opts = opts.unwrap_or_default();
        if opts.ctx.is_some() || opts.proxy.is_some() {
            return Err("ctx and proxy must be set in HttpClient.new".to_string());
        }
        let deadline = opts
            .timeout
            .and_then(timeout_from_ms)
            .map(|d| Instant::now() + d);
        match with_deadline(deadline, do_request(&self, method, url, opts, deadline)).await {
            Ok(res) => res,
            Err(()) => Err("timeout".to_string()),
        }
    }
}

async fn do_request(
    client: &HttpClient,
    method: String,
    url: String,
    opts: RequestOptions,
    deadline: Option<Instant>,
) -> Result<ClientResponse, String> {
    let mut method = Method::from_bytes(method.as_bytes())
        .map_err(|e| format!("invalid method '{method}': {e}"))?;
    let mut target = super::url::parse(&url)?;
    let mut headers = opts.headers;
    let mut body = opts.body;
    let max_body = opts.max_body.filter(|n| *n > 0).map(|n| n as u64);
    let mut redirects = 0i64;
    loop {
        let uri = http_uri(&target)?;
        let route = client.route(target.as_str()).await?;
        let pool = client.pool(route.as_ref());
        let proxy_auth = if target.scheme() == "http" {
            route
                .as_ref()
                .filter(|p| p.is_http())
                .and_then(Proxy::forward_auth)
        } else {
            None
        };
        let replay = body.repeat();
        let mut guard = BodyCleanup(Some(body.clone()));
        let request = build_request(&method, &uri, &headers, body, proxy_auth)?;
        let response = pool
            .request(request)
            .await
            .map_err(|e| format!("http: {}", describe(&e)))?;
        let status = response.status();
        let version = version_num(response.version());
        let (parts, incoming) = response.into_parts();
        let response_headers = Headers(
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
        );
        let incoming = Body::incoming(incoming, deadline, max_body, guard.0.take());
        let location = if is_redirect(status) && redirects < opts.redirects.max(0) {
            response_headers.get("location").map(str::to_owned)
        } else {
            None
        };
        let Some(location) = location else {
            let body = if opts.stream {
                BodyValue::Stream(incoming)
            } else {
                BodyValue::Bytes(LuaBytes(incoming.collect(None).await?))
            };
            return Ok(ClientResponse {
                status: status.as_u16(),
                version,
                headers: response_headers,
                body,
            });
        };
        if opts.stream {
            incoming.stop();
        } else {
            incoming.collect(None).await?;
        }
        let next = super::url::join(&target, &location)?;
        let to_get = status == StatusCode::SEE_OTHER
            || ((status == StatusCode::MOVED_PERMANENTLY || status == StatusCode::FOUND)
                && method != Method::GET
                && method != Method::HEAD);
        if to_get {
            method = Method::GET;
            body = BodyValue::default();
            headers.0.retain(|(k, _)| {
                k != "content-length" && k != "content-type" && k != "transfer-encoding"
            });
        } else {
            body = replay?;
        }
        if next.origin() != target.origin() {
            headers.0.retain(|(k, _)| {
                k != "authorization" && k != "cookie" && k != "proxy-authorization"
            });
        }
        target = next;
        redirects += 1;
    }
}
fn build_request(
    method: &Method,
    uri: &Uri,
    headers: &Headers,
    body: BodyValue,
    proxy_auth: Option<&str>,
) -> Result<Request<Outgoing>, String> {
    let length = body.length();
    let mut builder = Request::builder().method(method.clone()).uri(uri.clone());
    let map = builder.headers_mut().ok_or("invalid request")?;
    for (k, v) in &headers.0 {
        if k == "proxy-authorization" {
            return Err("proxy-authorization must be configured via Proxy.new".into());
        }
        if k == "transfer-encoding" {
            continue;
        }
        if k == "content-length" && v.parse::<u64>().ok() != length {
            return Err("content-length must match a body with known length".into());
        }
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| format!("invalid header name '{k}': {e}"))?;
        let value = HeaderValue::from_bytes(v.as_bytes())
            .map_err(|e| format!("invalid header value for '{k}': {e}"))?;
        map.append(name, value);
    }
    if let Some(auth) = proxy_auth {
        map.insert(
            "proxy-authorization",
            HeaderValue::from_str(auth).map_err(|_| "invalid proxy authorization")?,
        );
    }
    if !map.contains_key(ACCEPT_ENCODING) {
        map.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    }
    builder
        .body(body.into_wire()?)
        .map_err(|e| format!("build request: {e}"))
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

/// HTTP 不发送 fragment 或隐式解释 userinfo；凭证由显式请求头提供。
fn http_uri(url: &url::Url) -> Result<Uri, String> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("HTTP URL must use http or https and have a host".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "HTTP URL userinfo is not supported; use explicit authentication headers".into(),
        );
    }
    url[..url::Position::AfterQuery]
        .parse()
        .map_err(|err| format!("invalid HTTP URL: {err}"))
}

/// hyper 的错误 Display 只有分类名，真正的信息在 source 链上
fn describe(err: &(dyn StdError + 'static)) -> String {
    let mut msg = err.to_string();
    let mut src = err.source();
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}
