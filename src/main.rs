use cosmos_rs::{Machine, Network};

mod demo;

fn main() {
    let mut cli = std::env::args();
    let path = cli.nth(1).expect("usage: cosmos-rs <script.lua> [args...]");
    let script_args: Vec<String> = cli.collect();
    let code = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        std::process::exit(2);
    });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap_or_else(|error| {
        eprintln!("machine error: {error}");
        std::process::exit(1);
    });
    if let Err(error) = machine.add_module(Network) {
        drop(machine);
        eprintln!("network module error: {error}");
        std::process::exit(1);
    }
    machine.register::<demo::Counter>();

    // 测试用：echo 任务与 Lua 协程由同一个 Machine 的 LocalSet 驱动。
    if let Ok(port) = std::env::var("COSMOS_ECHO_PORT") {
        let port: u16 = port.parse().expect("invalid COSMOS_ECHO_PORT");
        machine.spawn(echo_server(port));
    }
    if let Ok(port) = std::env::var("COSMOS_TLS_ECHO_PORT") {
        let port: u16 = port.parse().expect("invalid COSMOS_TLS_ECHO_PORT");
        machine.spawn(tls_echo_server(port));
    }

    let result = runtime.block_on(machine.exec_with_args(&format!("@{path}"), &code, &script_args));
    drop(machine);
    if let Err(error) = result {
        eprintln!("lua error: {error}");
        std::process::exit(1);
    }
}

async fn echo_server(port: u16) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("echo server bind failed");
    loop {
        let (mut sock, _) = listener.accept().await.expect("echo accept failed");
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
}

/// 测试用 TLS echo server：证书为 examples/certs 下的自签 localhost 证书
/// （SAN: DNS:localhost, IP:127.0.0.1），客户端用同一 PEM 作为信任根
async fn tls_echo_server(port: u16) {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsAcceptor;

    let cert_pem: &[u8] = include_bytes!("../examples/certs/cert.pem");
    let key_pem: &[u8] = include_bytes!("../examples/certs/key.pem");
    let certs = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<Result<Vec<_>, _>>()
        .expect("bad test cert");
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .expect("bad test key")
        .expect("test key missing");
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("tls server config");
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("tls echo server bind failed");
    loop {
        let (sock, _) = listener.accept().await.expect("tls echo accept failed");
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            // 客户端拒绝证书时握手失败，直接丢弃这条连接
            let Ok(mut tls) = acceptor.accept(sock).await else {
                return;
            };
            let mut buf = [0u8; 1024];
            loop {
                match tls.read(&mut buf).await {
                    Ok(0) => {
                        // 对端发了 close_notify：回一个，让客户端看到干净 EOF
                        let _ = tls.shutdown().await;
                        break;
                    }
                    Err(_) => break,
                    Ok(n) => {
                        if tls.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
}
