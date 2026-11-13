//! TLS 模块：基于 rustls（ring 后端）的客户端与服务端流。
//!
//! Lua: local Tls = require("cosmos.network.tcp.tls")（对应 C++ cosmos.network.tcp.tls），
//!      local TlsContext = require("cosmos.network.tcp.tls.context")
//!
//! 配置走 TlsContext（对应 C++ 的 ssl_ctx）：信任根、verify_none、客户端证书、
//! ALPN 都在这里设置并缓存构建结果。Tls.connect 不传 context 时用缓存的
//! 默认配置（webpki-roots 公有根）。
//!
//! 连接成功后与 Tcp 完全同构：split() 得到同样的 TcpReader/TcpWriter
//! （内部为 TLS 读写半），receive 状态机、settimeout、setbufsize、receiveuntil
//! 全部复用。close() 会发 close_notify。

use std::cell::Cell;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::client::Resumption;
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use cosmos::ffi::lua_State;
use cosmos::userdata::{check, FromLua};

use crate::error::classify_io;
use crate::tcp::{
    size_limit, timeout_from_ms, with_deadline, ReadSide, State, TcpReader, TcpWriter, WriteSide,
};

/// 客户端的默认配置只构建一次（webpki 根证书 ~150 个锚点）
static DEFAULT_CLIENT: OnceLock<Arc<ClientConfig>> = OnceLock::new();

fn default_client_config() -> Arc<ClientConfig> {
    DEFAULT_CLIENT
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// TLS 流：客户端和服务端的 tokio-rustls 流是两个类型，统一成一个枚举，
/// 让 split 后的读写半（ReadSide/WriteSide::Tls）只有一种类型
pub enum TlsIo {
    Client(ClientTlsStream<TcpStream>),
    Server(ServerTlsStream<TcpStream>),
}

impl AsyncRead for TlsIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TlsIo::Client(s) => Pin::new(s).poll_read(cx, buf),
            TlsIo::Server(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl TlsIo {
    fn peer_cert(&self) -> Option<CertificateDer<'static>> {
        match self {
            TlsIo::Client(s) => s.get_ref().1.peer_certificates(),
            TlsIo::Server(s) => s.get_ref().1.peer_certificates(),
        }
        .and_then(|certs| certs.first().cloned())
    }

    /// 握手后 ALPN 是否协商为 h2（HTTP 客户端的 h2 连接池判断用）
    pub(crate) fn negotiated_h2(&self) -> bool {
        let alpn = match self {
            TlsIo::Client(s) => s.get_ref().1.alpn_protocol(),
            TlsIo::Server(s) => s.get_ref().1.alpn_protocol(),
        };
        alpn == Some(b"h2")
    }
}

impl AsyncWrite for TlsIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            TlsIo::Client(s) => Pin::new(s).poll_write(cx, buf),
            TlsIo::Server(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TlsIo::Client(s) => Pin::new(s).poll_flush(cx),
            TlsIo::Server(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TlsIo::Client(s) => Pin::new(s).poll_shutdown(cx),
            TlsIo::Server(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// 连接入口：静态方法
///   Tls.connect(host, port, timeout_ms?, ctx?)  → TlsConn
///   Tls.listen(port, ctx?, timeout_ms?)         → TlsListener
pub struct Tls;

#[cosmos::userdata(name = "network.tcp.tls")]
impl Tls {
    /// 客户端：TCP 连接 + TLS 握手（共用一个 deadline）。ctx 缺省用公有根证书
    async fn connect(
        host: String,
        port: i64,
        timeout_ms: Option<i64>,
        ctx: Option<ClientCfg>,
    ) -> Result<TlsConn, String> {
        let io = connect_tls_io(&host, port as u16, timeout_ms, ctx).await?;
        Ok(TlsConn {
            peer_cert: io.peer_cert(),
            stream: Some(io),
            state: Rc::new(Cell::new(State::Connected)),
            timeout: None,
            max_buf: None,
        })
    }

    /// 服务端：绑定端口，ctx 必须带证书链和私钥（TlsContext.new_server + use_*）
    async fn listen(
        port: i64,
        ctx: Option<ServerCfg>,
        timeout_ms: Option<i64>,
    ) -> Result<TlsListener, String> {
        let ctx = ctx.ok_or("Tls.listen requires a TlsContext with a server certificate")?;
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port as u16))
            .await
            .map_err(|e| classify_io(&e))?;
        Ok(TlsListener {
            listener: Some(listener),
            acceptor: TlsAcceptor::from(ctx.0),
            timeout: timeout_ms.and_then(timeout_from_ms),
        })
    }
}

/// 建立 TLS 客户端流（TCP + 握手共用一个 deadline）；Tls.connect 与 HTTP client 共用
pub(crate) async fn connect_tls_io(
    host: &str,
    port: u16,
    timeout_ms: Option<i64>,
    ctx: Option<ClientCfg>,
) -> Result<TlsIo, String> {
    let config = match ctx {
        Some(ClientCfg(c)) => c,
        None => default_client_config(),
    };
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| format!("invalid server name '{host}': {e}"))?;
    let deadline = timeout_ms
        .and_then(timeout_from_ms)
        .map(|d| Instant::now() + d);

    let tcp = match with_deadline(deadline, TcpStream::connect((host, port))).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(classify_io(&e)),
        Err(()) => return Err("timeout".to_string()),
    };
    let stream = match with_deadline(deadline, TlsConnector::from(config).connect(server_name, tcp))
        .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(classify_io(&e)),
        Err(()) => return Err("timeout".to_string()),
    };
    Ok(TlsIo::Client(stream))
}

/// 配置对象（对应 C++ ssl_ctx）：所有 setter 都会让已构建的配置失效，
/// 下次 connect/listen 时重建并缓存。
pub struct TlsContext {
    roots: RootCertStore,
    verify: bool,
    certs: Option<Vec<CertificateDer<'static>>>,
    key: Option<PrivateKeyDer<'static>>,
    alpn: Vec<Vec<u8>>,
    /// 服务端是否要求客户端证书（mTLS），信任根用 roots
    client_verify: bool,
    /// 会话复用：None=默认容量(256)，Some(0)=关闭，Some(n)=n 条
    resumption: Option<usize>,
    client_cfg: Option<Arc<ClientConfig>>,
    server_cfg: Option<Arc<ServerConfig>>,
}

#[cosmos::userdata(name = "network.tcp.tls.context")]
impl TlsContext {
    /// 客户端配置：webpki 公有根证书 + 校验开启
    fn new() -> TlsContext {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        TlsContext {
            roots,
            verify: true,
            certs: None,
            key: None,
            alpn: Vec::new(),
            client_verify: false,
            resumption: None,
            client_cfg: None,
            server_cfg: None,
        }
    }

    /// 服务端配置：不预置信任根（对端校验取决于是否要 mTLS，当前不启用），
    /// 需要 use_cert_chain_file + use_private_key_file
    fn new_server() -> TlsContext {
        TlsContext {
            roots: RootCertStore::empty(),
            verify: false,
            certs: None,
            key: None,
            alpn: Vec::new(),
            client_verify: false,
            resumption: None,
            client_cfg: None,
            server_cfg: None,
        }
    }

    /// 追加 PEM 文件中的证书到信任根（与 C++ add_ca_file 一致：追加而非替换）
    fn add_ca_file(&mut self, path: String) -> Result<(), String> {
        let pem = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
        self.add_pem(&pem)
    }

    /// 追加 PEM 字符串里的证书到信任根
    fn add_ca_pem(&mut self, pem: String) -> Result<(), String> {
        self.add_pem(pem.as_bytes())
    }

    /// 客户端证书链（PEM 文件，mTLS 用）
    fn use_cert_chain_file(&mut self, path: String) -> Result<(), String> {
        let pem = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
        let certs = rustls_pemfile::certs(&mut &pem[..])
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("{path}: {e}"))?;
        if certs.is_empty() {
            return Err(format!("{path}: no certificates found"));
        }
        self.certs = Some(certs);
        self.invalidate();
        Ok(())
    }

    /// 私钥（PEM 文件）
    fn use_private_key_file(&mut self, path: String) -> Result<(), String> {
        let pem = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
        let key = rustls_pemfile::private_key(&mut &pem[..])
            .map_err(|e| format!("{path}: {e}"))?
            .ok_or_else(|| format!("{path}: no private key found"))?;
        self.key = Some(key);
        self.invalidate();
        Ok(())
    }

    /// 关闭证书校验（对应 C++ verify_none；仅测试/内网用）
    fn verify_none(&mut self) {
        self.verify = false;
        self.invalidate();
    }

    /// 追加 ALPN 协议（如 "h2"、"http/1.1"）
    fn add_alpn(&mut self, proto: String) {
        self.alpn.push(proto.into_bytes());
        self.invalidate();
    }

    /// 批量加载目录下的 PEM 证书到信任根（非 PEM 文件跳过）
    fn add_ca_dir(&mut self, dir: String) -> Result<(), String> {
        let mut added = 0usize;
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("{dir}: {e}"))? {
            let path = entry.map_err(|e| format!("{dir}: {e}"))?.path();
            if !path.is_file() {
                continue;
            }
            let Ok(pem) = std::fs::read(&path) else {
                continue;
            };
            if self.add_pem(&pem).is_ok() {
                added += 1;
            }
        }
        if added == 0 {
            return Err(format!("{dir}: no CA certificates found"));
        }
        Ok(())
    }

    /// 服务端要求客户端出示证书（mTLS），用已加载的信任根校验
    fn verify_client(&mut self) {
        self.client_verify = true;
        self.invalidate();
    }

    /// 设置会话复用缓存容量（条）；0 关闭复用。缺省 256
    fn set_resumption_size(&mut self, n: i64) {
        self.resumption = Some(n.max(0) as usize);
        self.invalidate();
    }

    fn disable_resumption(&mut self) {
        self.resumption = Some(0);
        self.invalidate();
    }
}

impl TlsContext {
    fn invalidate(&mut self) {
        self.client_cfg = None;
        self.server_cfg = None;
    }

    fn add_pem(&mut self, pem: &[u8]) -> Result<(), String> {
        let mut added = 0;
        for cert in rustls_pemfile::certs(&mut &pem[..]) {
            let cert = cert.map_err(|e| e.to_string())?;
            self.roots.add(cert).map_err(|e| e.to_string())?;
            added += 1;
        }
        if added == 0 {
            return Err("no certificates found in PEM".to_string());
        }
        self.invalidate();
        Ok(())
    }

    fn client_config(&mut self) -> Result<Arc<ClientConfig>, String> {
        if let Some(cfg) = &self.client_cfg {
            return Ok(cfg.clone());
        }
        let builder = ClientConfig::builder();
        let builder = if self.verify {
            builder.with_root_certificates(self.roots.clone())
        } else {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify::new()))
        };
        let mut cfg = match (&self.certs, &self.key) {
            (Some(certs), Some(key)) => builder
                .with_client_auth_cert(certs.clone(), key.clone_key())
                .map_err(|e| e.to_string())?,
            (None, None) => builder.with_no_client_auth(),
            _ => return Err("client cert and private key must be set together".to_string()),
        };
        cfg.alpn_protocols = self.alpn.clone();
        cfg.resumption = match self.resumption {
            Some(0) => Resumption::disabled(),
            Some(n) => Resumption::in_memory_sessions(n),
            None => Resumption::in_memory_sessions(256),
        };
        let cfg = Arc::new(cfg);
        self.client_cfg = Some(cfg.clone());
        Ok(cfg)
    }

    fn server_config(&mut self) -> Result<Arc<ServerConfig>, String> {
        if let Some(cfg) = &self.server_cfg {
            return Ok(cfg.clone());
        }
        let (Some(certs), Some(key)) = (&self.certs, &self.key) else {
            return Err("server context requires a certificate chain and private key".to_string());
        };
        let builder = ServerConfig::builder();
        let builder = if self.client_verify {
            let verifier = WebPkiClientVerifier::builder(Arc::new(self.roots.clone()))
                .build()
                .map_err(|e| e.to_string())?;
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };
        let mut cfg = builder
            .with_single_cert(certs.clone(), key.clone_key())
            .map_err(|e| e.to_string())?;
        cfg.alpn_protocols = self.alpn.clone();
        let cfg = Arc::new(cfg);
        self.server_cfg = Some(cfg.clone());
        Ok(cfg)
    }
}

/// connect 的 ctx 参数：从 userdata 取（或懒构建）客户端配置快照
pub(crate) struct ClientCfg(pub(crate) Arc<ClientConfig>);

impl FromLua for ClientCfg {
    unsafe fn from_lua(L: *mut lua_State, idx: i32) -> Result<Self, String> {
        let mut ctx = check::<TlsContext>(L, idx);
        ctx.client_config().map(ClientCfg)
    }
}

/// listen 的 ctx 参数：同上，取服务端配置
pub(crate) struct ServerCfg(pub(crate) Arc<ServerConfig>);

impl FromLua for ServerCfg {
    unsafe fn from_lua(L: *mut lua_State, idx: i32) -> Result<Self, String> {
        let mut ctx = check::<TlsContext>(L, idx);
        ctx.server_config().map(ServerCfg)
    }
}

/// verify_none 用的空校验器：接受任意证书，但签名校验仍走 ring
#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl NoVerify {
    fn new() -> Self {
        Self(Arc::new(rustls::crypto::ring::default_provider()))
    }
}

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub struct TlsConn {
    stream: Option<TlsIo>,
    state: Rc<Cell<State>>,
    timeout: Option<Duration>,
    max_buf: Option<usize>,
    /// 握手完成时的对端叶证书（DER）；mTLS/客户端场景下可查
    peer_cert: Option<CertificateDer<'static>>,
}

#[cosmos::userdata(name = "network.tcp.tls.conn")]
impl TlsConn {
    fn state(&self) -> i64 {
        self.state.get() as i64
    }

    /// 对端叶证书（PEM）；没有（如 verify_none 之外的匿名连接）返回 nil
    fn getpeercert(&self) -> Option<String> {
        self.peer_cert.as_ref().map(|c| to_pem(c.as_ref()))
    }

    /// 设置读写超时（毫秒）；ms <= 0 表示不超时
    fn settimeout(&mut self, ms: i64) {
        self.timeout = timeout_from_ms(ms);
    }

    /// 设置读取缓冲上限（字节，*a/*l/*L/receiveuntil 生效）；n <= 0 不限
    fn setbufsize(&mut self, n: i64) {
        self.max_buf = size_limit(n);
    }

    /// 拆成 reader/writer：TLS 会话读写共用状态，走 tokio::io::split
    fn split(&mut self) -> (TcpReader, TcpWriter) {
        let stream = self.stream.take().expect("connection already split");
        let (r, w) = tokio::io::split(stream);
        (
            TcpReader {
                inner: Some(BufReader::new(ReadSide::Tls(r))),
                state: self.state.clone(),
                buf: Vec::new(),
                timeout: self.timeout,
                max_buf: self.max_buf,
            },
            TcpWriter {
                inner: Some(WriteSide::Tls(w)),
                state: self.state.clone(),
                timeout: self.timeout,
            },
        )
    }

    /// 关闭连接：先发 close_notify（rustls poll_shutdown），再 drop 底层流
    async fn close(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            let deadline = self.timeout.map(|d| Instant::now() + d);
            let _ = with_deadline(deadline, stream.shutdown()).await;
        }
        self.state.set(State::Closed);
    }
}

/// TLS 监听器：accept() 返回与客户端同构的 TlsConn（内部为服务端 TLS 流）
pub struct TlsListener {
    listener: Option<tokio::net::TcpListener>,
    acceptor: TlsAcceptor,
    timeout: Option<Duration>,
}

#[cosmos::userdata(name = "network.tcp.tls.listener")]
impl TlsListener {
    /// 接受一个连接并完成握手；timeout 覆盖 accept + 握手
    async fn accept(&mut self) -> Result<TlsConn, String> {
        let Some(listener) = self.listener.as_ref() else {
            return Err("closed".to_string());
        };
        let deadline = self.timeout.map(|d| Instant::now() + d);
        let (sock, _) = match with_deadline(deadline, listener.accept()).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(classify_io(&e)),
            Err(()) => return Err("timeout".to_string()),
        };
        let stream = match with_deadline(deadline, self.acceptor.accept(sock)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(classify_io(&e)),
            Err(()) => return Err("timeout".to_string()),
        };
        let io = TlsIo::Server(stream);
        Ok(TlsConn {
            peer_cert: io.peer_cert(),
            stream: Some(io),
            state: Rc::new(Cell::new(State::Connected)),
            timeout: None,
            max_buf: None,
        })
    }

    /// 热替换服务端配置：只影响之后 accept 的连接（已有连接保持旧证书）
    fn reload(&mut self, ctx: Option<ServerCfg>) -> Result<(), String> {
        let ctx = ctx.ok_or("reload requires a TlsContext with a server certificate")?;
        if self.listener.is_none() {
            return Err("closed".to_string());
        }
        self.acceptor = TlsAcceptor::from(ctx.0);
        Ok(())
    }

    fn close(&mut self) {
        self.listener.take();
    }
}

/// DER → PEM（手写 base64，避免额外依赖）
fn to_pem(der: &[u8]) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(der.len() * 2 + 64);
    out.push_str("-----BEGIN CERTIFICATE-----\n");
    for chunk in der.chunks(48) {
        let mut line = String::with_capacity(64);
        for triple in chunk.chunks(3) {
            let n = ((triple[0] as u32) << 16)
                | ((triple.get(1).copied().unwrap_or(0) as u32) << 8)
                | triple.get(2).copied().unwrap_or(0) as u32;
            line.push(B64[(n >> 18) as usize] as char);
            line.push(B64[((n >> 12) & 63) as usize] as char);
            line.push(if triple.len() > 1 {
                B64[((n >> 6) & 63) as usize] as char
            } else {
                '='
            });
            line.push(if triple.len() > 2 {
                B64[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

pub unsafe fn register(L: *mut lua_State) {
    cosmos::userdata::register::<Tls>(L);
    cosmos::userdata::register::<TlsContext>(L);
    cosmos::userdata::register::<TlsConn>(L);
    cosmos::userdata::register::<TlsListener>(L);
}
