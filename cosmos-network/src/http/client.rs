//! HTTP 客户端：hyper + hyper-util 连接池，外挂重定向与整体超时。
//!
//! Lua 侧：
//!   local HttpClient = require("cosmos.network.http.client")
//!   local resp, err = HttpClient.request("GET", "http://host/path", {
//!     headers = { ["x-a"] = "1" },
//!     body = "...",          -- 任意方法都可带 body
//!     timeout = 3000,        -- 毫秒，覆盖连接+重定向+读完 body
//!     ctx = tls_ctx,         -- 可选 TlsContext（信任根/客户端证书/ALPN）
//!     redirects = 5,         -- 最多跟随几次重定向，0 关闭
//!     max_body = 1048576,    -- 可选：响应体上限，超过返回错误
//!   })
//!   -- resp = { status=200, version=11, headers={...}, body="..." }
//!
//! 连接池按 TlsContext 的配置对象（Arc 指针）缓存：TLS 配置绑定在 connector 上，
//! 不同信任根不能共用同一个池。默认（不传 ctx）池用 webpki 公有根 + ALPN h2/http1.1。
//! v1 不处理 gzip（固定发 Accept-Encoding: identity）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::header::{HeaderName, HeaderValue, ACCEPT_ENCODING};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio::time::Instant;

use super::{version_num, Conn, Headers, LuaBytes};
use crate::error::classify_io;
use crate::tcp::stream::{timeout_from_ms, with_deadline};
use crate::tcp::tls::{connect_tls_io, ClientCfg};

type BoxErr = Box<dyn StdError + Send + Sync>;
type HttpPool = Client<Connector, Full<Bytes>>;

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

// 连接池按 TLS 配置缓存。键为 TlsContext 缓存的 Arc 指针（0 = 默认配置）：
// 配置一变 TlsContext 就重建 Arc，自然落到新的池，旧连接不会串用新信任根
thread_local! {
    static CLIENTS: RefCell<HashMap<usize, HttpPool>> = RefCell::new(HashMap::new());
}

fn client_for(tls: Option<Arc<ClientConfig>>) -> HttpPool {
    let key = tls.as_ref().map(|c| Arc::as_ptr(c) as usize).unwrap_or(0);
    if let Some(c) = CLIENTS.with(|m| m.borrow().get(&key).cloned()) {
        return c;
    }
    let cfg = match tls {
        None => default_tls_config(),
        Some(c) if c.alpn_protocols.is_empty() => {
            let mut c = (*c).clone();
            c.alpn_protocols = default_alpn();
            Arc::new(c)
        }
        Some(c) => c,
    };
    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .build::<Connector, Full<Bytes>>(Connector { tls: Some(cfg) });
    CLIENTS.with(|m| m.borrow_mut().insert(key, client.clone()));
    client
}

/// connector：按 URI scheme 选择明文 TCP 或 TLS，返回统一的 Conn
#[derive(Clone)]
struct Connector {
    tls: Option<Arc<ClientConfig>>,
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
                let cfg = tls.ok_or_else(|| -> BoxErr { "https requires TLS support".into() })?;
                let io = connect_tls_io(host, port, None, Some(ClientCfg(cfg)))
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

/// request 的第三个参数（options table）。
/// 表的逐字段读取由 `#[derive(FromLua)]` 生成（见 cosmos-macros）；
/// `#[lua(default)]` 表示整张表缺省时按 `Default` 取同名字段。
#[derive(cosmos::FromLua)]
#[lua(default)]
pub(crate) struct ClientOptions {
    headers: Headers,
    body: LuaBytes,
    timeout: Option<i64>,
    ctx: Option<ClientCfg>,
    redirects: i64,
    max_body: Option<i64>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            headers: Headers::default(),
            body: LuaBytes::default(),
            timeout: None,
            ctx: None,
            redirects: 5,
            max_body: None,
        }
    }
}

/// 客户端响应（Lua 侧 table），压栈由 `#[derive(PushLua)]` 生成
#[derive(cosmos::PushLua)]
pub(crate) struct ClientResponse {
    status: u16,
    version: i64,
    headers: Headers,
    body: LuaBytes,
}

/// 客户端入口：静态方法
///   HttpClient.request(method, url, opts?) → resp
pub struct HttpClient;

#[cosmos::userdata(name = "network.http.client")]
impl HttpClient {
    /// 发一次请求（含重定向跟随）；失败返回 nil + 错误串
    async fn request(
        method: String,
        url: String,
        opts: Option<ClientOptions>,
    ) -> Result<ClientResponse, String> {
        let opts = opts.unwrap_or_default();
        let deadline = opts
            .timeout
            .and_then(timeout_from_ms)
            .map(|d| Instant::now() + d);
        match with_deadline(deadline, do_request(method, url, opts)).await {
            Ok(res) => res,
            Err(()) => Err("timeout".to_string()),
        }
    }
}

async fn do_request(
    method: String,
    url: String,
    opts: ClientOptions,
) -> Result<ClientResponse, String> {
    let mut method = Method::from_bytes(method.as_bytes())
        .map_err(|e| format!("invalid method '{method}': {e}"))?;
    let mut uri: Uri = url
        .parse()
        .map_err(|e| format!("invalid url '{url}': {e}"))?;
    let mut headers = opts.headers;
    let mut body = opts.body.0;
    let max_redirects = opts.redirects.max(0);
    let mut redirects = 0i64;

    loop {
        let client = client_for(opts.ctx.as_ref().map(|c| c.0.clone()));
        let request = build_request(&method, &uri, &headers, &body)?;
        let resp = client
            .request(request)
            .await
            .map_err(|e| format!("http: {}", describe(&e)))?;

        let status = resp.status();
        let version = version_num(resp.version());
        let (parts, incoming) = resp.into_parts();
        let resp_headers = Headers(
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
        let resp_body = match opts.max_body.filter(|n| *n > 0) {
            Some(limit) => Limited::new(incoming, limit as usize)
                .collect()
                .await
                .map_err(|e| format!("http body: {}", describe(&*e)))?
                .to_bytes(),
            None => incoming
                .collect()
                .await
                .map_err(|e| format!("http body: {}", describe(&e)))?
                .to_bytes(),
        };

        let location = if is_redirect(status) {
            resp_headers.get("location").map(str::to_string)
        } else {
            None
        };
        let Some(location) = location else {
            return Ok(ClientResponse {
                status: status.as_u16(),
                version,
                headers: resp_headers,
                body: LuaBytes(resp_body),
            });
        };
        if redirects >= max_redirects {
            return Ok(ClientResponse {
                status: status.as_u16(),
                version,
                headers: resp_headers,
                body: LuaBytes(resp_body),
            });
        }

        let next = resolve(&uri, &location)
            .ok_or_else(|| format!("invalid redirect location '{location}'"))?;
        // 303 一律转 GET；301/302 对非 GET/HEAD 按浏览器惯例转 GET；307/308 保留原样
        let to_get = status == StatusCode::SEE_OTHER
            || ((status == StatusCode::MOVED_PERMANENTLY || status == StatusCode::FOUND)
                && method != Method::GET
                && method != Method::HEAD);
        if to_get {
            method = Method::GET;
            body = Bytes::new();
            headers
                .0
                .retain(|(k, _)| k != "content-length" && k != "content-type" && k != "transfer-encoding");
        }
        if origin(&next) != origin(&uri) {
            headers.0.retain(|(k, _)| {
                k != "authorization" && k != "cookie" && k != "proxy-authorization"
            });
        }
        uri = next;
        redirects += 1;
    }
}

fn build_request(
    method: &Method,
    uri: &Uri,
    headers: &Headers,
    body: &Bytes,
) -> Result<Request<Full<Bytes>>, String> {
    let mut request = Request::builder()
        .method(method.clone())
        .uri(uri.clone())
        .body(Full::new(body.clone()))
        .map_err(|e| format!("build request: {e}"))?;
    {
        let map = request.headers_mut();
        for (k, v) in &headers.0 {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| format!("invalid header name '{k}'"))?;
            let value = HeaderValue::from_bytes(v.as_bytes())
                .map_err(|_| format!("invalid header value for '{k}'"))?;
            map.append(name, value);
        }
        if !map.contains_key(ACCEPT_ENCODING) {
            map.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        }
    }
    Ok(request)
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

/// (scheme, authority) —— 跨域时剥掉凭证头
fn origin(uri: &Uri) -> (Option<&str>, Option<&str>) {
    (uri.scheme_str(), uri.authority().map(|a| a.as_str()))
}

/// Location 只做常见形态：绝对 URL、以 / 开头的绝对路径、相对路径、仅 query
fn resolve(base: &Uri, loc: &str) -> Option<Uri> {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.parse().ok();
    }
    let scheme = base.scheme_str()?;
    // 协议相对：//host/path 沿用当前 scheme
    if let Some(rest) = loc.strip_prefix("//") {
        return format!("{scheme}://{rest}").parse().ok();
    }
    let authority = base.authority()?.as_str();
    let path = if loc.starts_with('/') {
        loc.to_string()
    } else if loc.starts_with('?') {
        format!("{}{}", base.path(), loc)
    } else {
        let base_path = base.path();
        let dir = match base_path.rfind('/') {
            Some(i) => &base_path[..=i],
            None => "/",
        };
        format!("{dir}{loc}")
    };
    format!("{scheme}://{authority}{path}").parse().ok()
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
