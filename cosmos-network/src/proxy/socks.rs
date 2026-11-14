use super::transport::Io;
use std::net::{IpAddr, SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
pub(super) enum Failure {
    Io(std::io::Error),
    Status(u8),
    Protocol(&'static str),
}
impl From<std::io::Error> for Failure {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}
impl Failure {
    pub(super) fn retry(&self) -> bool {
        matches!(self, Self::Status(3 | 4 | 5 | 6 | 8))
    }
    pub(super) fn message(self) -> String {
        match self {
            Self::Io(e) => super::io_error(e),
            Self::Status(code) => format!("SOCKS5 proxy reply {code}"),
            Self::Protocol(message) => message.into(),
        }
    }
}
pub(super) enum Target<'a> {
    Address(SocketAddr),
    Domain(&'a str, u16),
}
pub(super) async fn connect(
    mut io: Io,
    target: Target<'_>,
    auth: Option<&(Vec<u8>, Vec<u8>)>,
) -> Result<Io, Failure> {
    let method = if auth.is_some() { 2 } else { 0 };
    io.write_all(&[5, 1, method]).await?;
    let mut reply = [0u8; 2];
    io.read_exact(&mut reply).await?;
    if reply != [5, method] {
        return Err(Failure::Protocol(
            "SOCKS5 proxy rejected the requested authentication method",
        ));
    }
    if let Some((user, password)) = auth {
        let mut request = vec![1, user.len() as u8];
        request.extend_from_slice(user);
        request.push(password.len() as u8);
        request.extend_from_slice(password);
        io.write_all(&request).await?;
        io.read_exact(&mut reply).await?;
        if reply != [1, 0] {
            return Err(Failure::Protocol("SOCKS5 authentication failed"));
        }
    }
    let mut request = vec![5, 1, 0];
    let port = match target {
        Target::Address(address) => {
            match address.ip() {
                IpAddr::V4(ip) => {
                    request.push(1);
                    request.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    request.push(4);
                    request.extend_from_slice(&ip.octets());
                }
            }
            address.port()
        }
        Target::Domain(host, port) => {
            if host.is_empty() || host.len() > 255 {
                return Err(Failure::Protocol(
                    "SOCKS5 target domain must contain 1..=255 bytes",
                ));
            }
            request.extend_from_slice(&[3, host.len() as u8]);
            request.extend_from_slice(host.as_bytes());
            port
        }
    };
    request.extend_from_slice(&port.to_be_bytes());
    io.write_all(&request).await?;
    let mut header = [0u8; 4];
    io.read_exact(&mut header).await?;
    if header[0] != 5 || header[2] != 0 {
        return Err(Failure::Protocol("invalid SOCKS5 response header"));
    }
    if header[1] != 0 {
        return Err(Failure::Status(header[1]));
    }
    let count = match header[3] {
        1 => 6,
        4 => 18,
        3 => {
            let len = io.read_u8().await?;
            if len == 0 {
                return Err(Failure::Protocol("empty SOCKS5 bound domain"));
            }
            usize::from(len) + 2
        }
        _ => return Err(Failure::Protocol("invalid SOCKS5 address type")),
    };
    io.read_exact(&mut [0u8; 257][..count]).await?;
    Ok(io)
}
