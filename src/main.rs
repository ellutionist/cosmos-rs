use std::ffi::CString;

use cosmos::coroutine::LuaCoroutine;
use cosmos::ffi::*;
use cosmos::{libs, stack, userdata};

mod demo;

fn main() {
    let mut cli = std::env::args();
    let path = cli.nth(1).expect("usage: cosmos-rs <script.lua> [args...]");
    let script_args: Vec<String> = cli.collect();
    let code = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        std::process::exit(2);
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .enable_io()
        .build()
        .unwrap();

    // 测试用：设置 COSMOS_ECHO_PORT / COSMOS_TLS_ECHO_PORT 时起 echo server
    if let Ok(port) = std::env::var("COSMOS_ECHO_PORT") {
        let port: u16 = port.parse().expect("invalid COSMOS_ECHO_PORT");
        rt.spawn(echo_server(port));
    }
    if let Ok(port) = std::env::var("COSMOS_TLS_ECHO_PORT") {
        let port: u16 = port.parse().expect("invalid COSMOS_TLS_ECHO_PORT");
        rt.spawn(tls_echo_server(port));
    }

    match rt.block_on(run(&code, &script_args)) {
        Ok(()) => {}
        Err(msg) => {
            eprintln!("lua error: {msg}");
            std::process::exit(1);
        }
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

async fn run(code: &str, args: &[String]) -> Result<(), String> {
    unsafe {
        let L = luaL_newstate();
        if L.is_null() {
            return Err("failed to create lua state".to_string());
        }
        luaL_openlibs(L);
        libs::open_cosmos(L);
        userdata::init(L);
        demo::register(L);
        cosmos_ext_demo::register(L);
        cosmos_network::register(L);

        // 各扩展 crate 的 Lua 部分（include_str! 编进二进制）：
        // 必须在 userdata 注册之后执行，require("cosmos.X") 才拿得到 metatable
        for (name, src) in cosmos_network::LUA_SOURCES {
            if let Err(e) = libs::run_lua_source(L, name, src) {
                lua_close(L);
                return Err(e);
            }
        }

        // 脚本跑在独立协程上（主栈只承载全局状态），这样 sleep 才能 yield
        let thread = lua_newthread(L);

        let ccode = CString::new(code).unwrap();
        if luaL_loadstring(thread, ccode.as_ptr()) != 0 {
            let msg = stack::err_message(thread);
            lua_close(L);
            return Err(msg);
        }

        // 初始参数：压在 chunk 函数之上，首次 resume 按 gettop - 1 传给脚本的 `...`
        for arg in args {
            lua_pushlstring(thread, arg.as_ptr() as *const cosmos::ffi::c_char, arg.len());
        }

        let result = LuaCoroutine::new(L, thread).await;
        userdata::cleanup(L);
        lua_close(L);
        result
    }
}
