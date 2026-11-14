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

use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::header::{HeaderName, HeaderValue, ACCEPT_ENCODING};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio::time::Instant;

use super::body::{Body, BodyCleanup, BodyValue, Outgoing};
use super::{version_num, Conn, Headers, LuaBytes};
use crate::deadline::{timeout_from_ms, with_deadline};
use crate::error::classify_io;
use crate::tcp::tls::{connect_tls_io, ClientCfg};

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

/// connector：按 URI scheme 选择明文 TCP 或 TLS，返回统一的 Conn
#[derive(Clone)]
struct Connector {
    tls: Arc<ClientConfig>,
}

impl tower_service::Service<Uri> for Connector {
    type Response = TokioIo<Conn>;
    type Error = BoxErr;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let tls = self.tls.clone();
        Box::pin(async move {
            let https = uri.scheme_str() == Some("https");
            let host = uri
                .host()
                .ok_or_else(|| -> BoxErr { "missing host in request URI".into() })?;
            // Uri::host 对 IPv6 带方括号，getaddrinfo / SNI 都要去掉
            let host = host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host);
            let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
            if https {
                let io = connect_tls_io(host, port, None, Some(ClientCfg(tls)))
                    .await
                    .map_err(|e| -> BoxErr { e.into() })?;
                Ok(TokioIo::new(Conn::Tls(io)))
            } else {
                let stream = TcpStream::connect((host, port))
                    .await
                    .map_err(|e| -> BoxErr { classify_io(&e).into() })?;
                Ok(TokioIo::new(Conn::Plain(stream)))
            }
        })
    }
}

/// 构造时固定的 TLS 配置，由该 client 的全部请求与重定向共用。
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct ClientOptions {
    ctx: Option<ClientCfg>,
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

/// 一个 client 对应一个连接池；拥有型句柄让并发请求不占用 userdata 借用。
#[derive(Clone)]
pub(crate) struct HttpClient {
    pool: HttpPool,
}

#[cosmos::userdata(name = "network.http.client")]
impl HttpClient {
    fn new(opts: Option<ClientOptions>) -> Self {
        let cfg = match opts.unwrap_or_default().ctx {
            None => default_tls_config(),
            Some(ClientCfg(cfg)) if cfg.alpn_protocols.is_empty() => {
                let mut cfg = (*cfg).clone();
                cfg.alpn_protocols = default_alpn();
                Arc::new(cfg)
            }
            Some(ClientCfg(cfg)) => cfg,
        };
        Self {
            pool: Client::builder(TokioExecutor::new())
                .pool_idle_timeout(Duration::from_secs(30))
                .pool_timer(TokioTimer::new())
                .pool_max_idle_per_host(32)
                .build(Connector { tls: cfg }),
        }
    }

    /// 发一次请求（含重定向跟随）；失败返回 nil + 错误串
    async fn request(
        self,
        method: String,
        url: String,
        opts: Option<RequestOptions>,
    ) -> Result<ClientResponse, String> {
        let opts = opts.unwrap_or_default();
        if opts.ctx.is_some() {
            return Err("ctx must be set in HttpClient.new".to_string());
        }
        let deadline = opts
            .timeout
            .and_then(timeout_from_ms)
            .map(|d| Instant::now() + d);
        match with_deadline(
            deadline,
            do_request(&self.pool, method, url, opts, deadline),
        )
        .await
        {
            Ok(res) => res,
            Err(()) => Err("timeout".to_string()),
        }
    }
}

async fn do_request(
    client: &HttpPool,
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
        let replay = body.repeat();
        let mut guard = BodyCleanup(Some(body.clone()));
        let request = build_request(&method, &uri, &headers, body)?;
        let response = client
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
) -> Result<Request<Outgoing>, String> {
    let length = body.length();
    let mut builder = Request::builder().method(method.clone()).uri(uri.clone());
    let map = builder.headers_mut().ok_or("invalid request")?;
    for (k, v) in &headers.0 {
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
