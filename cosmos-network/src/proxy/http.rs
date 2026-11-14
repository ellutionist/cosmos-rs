use super::transport::Io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
pub(super) async fn tunnel(mut io: Io, authority: &str, auth: Option<&str>) -> Result<Io, String> {
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(auth) = auth {
        request.push_str("Proxy-Authorization: ");
        request.push_str(auth);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    io.write_all(request.as_bytes())
        .await
        .map_err(super::io_error)?;
    let mut bytes = Vec::new();
    let mut consumed = 0usize;
    let mut interim = 0;
    loop {
        if let Some(end) = bytes
            .windows(4)
            .position(|b| b == b"\r\n\r\n")
            .map(|n| n + 4)
        {
            consumed += end;
            if consumed > 16384 {
                return Err("HTTP proxy response headers exceed 16 KiB".into());
            }
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut response = httparse::Response::new(&mut headers);
            response
                .parse(&bytes[..end])
                .map_err(|_| "invalid HTTP proxy response")?;
            let status = response.code.ok_or("missing HTTP proxy status")?;
            if (100..200).contains(&status) && status != 101 {
                interim += 1;
                if interim > 4 {
                    return Err("too many HTTP proxy interim responses".into());
                }
                bytes.drain(..end);
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(format!("HTTP proxy CONNECT failed with status {status}"));
            }
            return Ok(io.prefixed(bytes.split_off(end)));
        }
        let remaining = 16384usize.saturating_sub(consumed + bytes.len());
        if remaining == 0 {
            return Err("HTTP proxy response headers exceed 16 KiB".into());
        }
        let mut chunk = [0u8; 1024];
        let n = io
            .read(&mut chunk[..remaining.min(1024)])
            .await
            .map_err(super::io_error)?;
        if n == 0 {
            return Err("HTTP proxy closed during CONNECT".into());
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};
    #[tokio::test]
    async fn prefetched_tunnel_data_is_preserved_and_headers_are_bounded() {
        for oversized in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut io, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    bytes.push(io.read_u8().await.unwrap());
                }
                let response = if oversized {
                    format!("HTTP/1.1 200 OK\r\nx: {}", "x".repeat(17000)).into_bytes()
                } else {
                    b"HTTP/1.1 200 OK\r\n\r\npayload".to_vec()
                };
                let _ = io.write_all(&response).await;
            });
            let result = tunnel(
                Io::Tcp(TcpStream::connect(address).await.unwrap()),
                "example.com:443",
                None,
            )
            .await;
            if oversized {
                assert!(result.is_err());
            } else {
                let mut io = result.unwrap();
                let mut data = [0u8; 7];
                io.read_exact(&mut data).await.unwrap();
                assert_eq!(&data, b"payload");
            }
            server.await.unwrap();
        }
    }
}
