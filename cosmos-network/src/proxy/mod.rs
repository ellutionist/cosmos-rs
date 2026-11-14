//! 代理仅描述连接与认证；是否直连由应用的逐跳路由回调决定。
mod http;
mod socks;
mod transport;
use crate::tcp::tls::{client_server_name, with_alpn_client, ClientCfg};
use base64::Engine;
use cosmos::userdata::LuaBytes;
use rustls::ClientConfig;
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::net::{lookup_host, TcpStream};
use transport::Io;
pub(crate) use transport::Transport;
#[derive(Clone)]
pub(crate) struct Proxy(Arc<Config>);
struct Config {
    scheme: Kind,
    host: String,
    port: u16,
    credentials: Option<(Vec<u8>, Vec<u8>)>,
    basic: Option<String>,
    tls: Arc<ClientConfig>,
}
#[derive(Clone, Copy)]
enum Kind {
    Http,
    Https,
    Socks5,
    Socks5h,
}
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct Options {
    ctx: Option<ClientCfg>,
}
fn io_error(error: std::io::Error) -> String {
    crate::error::classify_io(&error)
}
fn credential(value: &str) -> Result<Vec<u8>, String> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err("invalid proxy credentials encoding".into());
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(percent_encoding::percent_decode_str(value).collect())
}
impl Proxy {
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn forward_auth(&self) -> Option<&str> {
        self.0.basic.as_deref()
    }
    pub(crate) fn is_http(&self) -> bool {
        matches!(self.0.scheme, Kind::Http | Kind::Https)
    }
    async fn endpoint(&self) -> Result<Io, String> {
        let io = Io::Tcp(
            TcpStream::connect((self.0.host.as_str(), self.0.port))
                .await
                .map_err(io_error)?,
        );
        if matches!(self.0.scheme, Kind::Https) {
            tls(io, &self.0.host, self.0.tls.clone())
                .await
                .map(|(io, _)| io)
        } else {
            Ok(io)
        }
    }
    async fn dial(&self, host: &str, port: u16, tunnel: bool) -> Result<Io, String> {
        match self.0.scheme {
            Kind::Http | Kind::Https => {
                let io = self.endpoint().await?;
                if tunnel {
                    let authority = if host.contains(':') {
                        format!("[{host}]:{port}")
                    } else {
                        format!("{host}:{port}")
                    };
                    http::tunnel(io, &authority, self.0.basic.as_deref()).await
                } else {
                    Ok(io)
                }
            }
            Kind::Socks5 | Kind::Socks5h => {
                if let Ok(ip) = host.parse::<IpAddr>() {
                    return socks::connect(
                        self.endpoint().await?,
                        socks::Target::Address(SocketAddr::new(ip, port)),
                        self.0.credentials.as_ref(),
                    )
                    .await
                    .map_err(socks::Failure::message);
                }
                if matches!(self.0.scheme, Kind::Socks5h) {
                    return socks::connect(
                        self.endpoint().await?,
                        socks::Target::Domain(host, port),
                        self.0.credentials.as_ref(),
                    )
                    .await
                    .map_err(socks::Failure::message);
                }
                let mut last = "SOCKS5 target resolved to no addresses".to_owned();
                for address in lookup_host((host, port)).await.map_err(io_error)?.take(16) {
                    match socks::connect(
                        self.endpoint().await?,
                        socks::Target::Address(address),
                        self.0.credentials.as_ref(),
                    )
                    .await
                    {
                        Ok(io) => return Ok(io),
                        Err(err) => {
                            let retry = err.retry();
                            last = err.message();
                            if !retry {
                                return Err(last);
                            }
                        }
                    }
                }
                Err(last)
            }
        }
    }
}
#[cosmos::userdata(name = "network.proxy")]
impl Proxy {
    fn new(value: LuaBytes, options: Option<Options>) -> Result<Self, String> {
        if value.len() > 4096 {
            return Err("proxy URL exceeds 4096 bytes".into());
        }
        let raw = std::str::from_utf8(&value).map_err(|_| "proxy URL must be valid UTF-8")?;
        let url = url::Url::parse(raw).map_err(|_| "invalid proxy URL (credentials hidden)")?;
        let scheme = match url.scheme() {
            "http" => Kind::Http,
            "https" => Kind::Https,
            "socks5" => Kind::Socks5,
            "socks5h" => Kind::Socks5h,
            _ => return Err("proxy scheme must be http, https, socks5 or socks5h".into()),
        };
        if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
            return Err("proxy URL must not contain a path, query or fragment".into());
        }
        let raw_host = url.host_str().ok_or("proxy URL requires a host")?;
        let decoded = percent_encoding::percent_decode_str(raw_host)
            .decode_utf8()
            .map_err(|_| "invalid proxy host")?;
        let host = url::Host::parse(&decoded)
            .map_err(|_| "invalid proxy host")?
            .to_string();
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = url.port().unwrap_or(match scheme {
            Kind::Http => 80,
            Kind::Https => 443,
            _ => 1080,
        });
        if port == 0 {
            return Err("proxy port must be in 1..=65535".into());
        }
        let credentials = if !url.username().is_empty() || url.password().is_some() {
            Some((
                credential(url.username())?,
                credential(url.password().unwrap_or(""))?,
            ))
        } else {
            None
        };
        if let Some((user, password)) = &credentials {
            if user.len() + password.len() > 1024 {
                return Err("proxy credentials exceed 1024 bytes".into());
            }
            if matches!(scheme, Kind::Socks5 | Kind::Socks5h)
                && (user.is_empty()
                    || password.is_empty()
                    || user.len() > 255
                    || password.len() > 255)
            {
                return Err("SOCKS5 username and password must each contain 1..=255 bytes".into());
            }
            if matches!(scheme, Kind::Http | Kind::Https) && user.contains(&b':') {
                return Err("HTTP proxy username cannot contain a colon".into());
            }
        }
        let basic = if matches!(scheme, Kind::Http | Kind::Https) {
            credentials.as_ref().map(|(user, password)| {
                let mut bytes = user.clone();
                bytes.push(b':');
                bytes.extend_from_slice(password);
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(bytes)
                )
            })
        } else {
            None
        };
        let ctx = options.unwrap_or_default().ctx;
        if ctx.is_some() && !matches!(scheme, Kind::Https) {
            return Err("proxy ctx requires an https proxy".into());
        }
        let tls = with_alpn_client(
            ctx.map_or_else(crate::http::client::default_tls_config, |ctx| ctx.0),
            &[b"http/1.1"],
        );
        Ok(Self(Arc::new(Config {
            scheme,
            host,
            port,
            credentials,
            basic,
            tls,
        })))
    }
    fn scheme(&self) -> &'static str {
        match self.0.scheme {
            Kind::Http => "http",
            Kind::Https => "https",
            Kind::Socks5 => "socks5",
            Kind::Socks5h => "socks5h",
        }
    }
    fn host(&self) -> &str {
        &self.0.host
    }
    fn port(&self) -> u16 {
        self.0.port
    }
}
async fn tls(io: Io, host: &str, config: Arc<ClientConfig>) -> Result<(Io, bool), String> {
    let stream = tokio_rustls::TlsConnector::from(config)
        .connect(client_server_name(host)?, io)
        .await
        .map_err(io_error)?;
    let h2 = stream.get_ref().1.alpn_protocol() == Some(b"h2");
    Ok((Io::Tls(Box::new(stream)), h2))
}
pub(crate) async fn connect(
    host: &str,
    port: u16,
    origin_tls: Option<Arc<ClientConfig>>,
    proxy: Option<&Proxy>,
    force_tunnel: bool,
) -> Result<Transport, String> {
    let forward = proxy.is_some_and(Proxy::is_http) && origin_tls.is_none() && !force_tunnel;
    let io = match proxy {
        Some(proxy) => proxy.dial(host, port, !forward).await?,
        None => Io::Tcp(TcpStream::connect((host, port)).await.map_err(io_error)?),
    };
    let (io, h2) = if let Some(config) = origin_tls {
        tls(io, host, config).await?
    } else {
        (io, false)
    };
    Ok(Transport { io, h2, forward })
}
