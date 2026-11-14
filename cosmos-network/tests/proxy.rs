//! 原生协议测试代理：真实 TLS 嵌套、SOCKS DNS/认证、HTTP CONNECT 与取消。
use cosmos::Machine;
use cosmos_network::Network;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};
fn runtime(multi: bool) -> tokio::runtime::Runtime {
    let mut b = if multi {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    b.worker_threads(2).enable_all().build().unwrap()
}
async fn execute(machine: &mut Machine, source: &str) {
    tokio::time::timeout(Duration::from_secs(20), machine.exec(source))
        .await
        .unwrap()
        .unwrap();
}
fn server_tls() -> Arc<rustls::ServerConfig> {
    let certs =
        rustls_pemfile::certs(&mut include_bytes!("../../examples/certs/cert.pem").as_slice())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
    let key =
        rustls_pemfile::private_key(&mut include_bytes!("../../examples/certs/key.pem").as_slice())
            .unwrap()
            .unwrap();
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(cfg)
}
async fn header(io: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(io.read_u8().await.unwrap());
        assert!(bytes.len() < 20000);
    }
    bytes
}
struct Fixture {
    port: u16,
    task: JoinHandle<()>,
    seen: Arc<Mutex<Vec<String>>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn http_proxy(secure: bool) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let task = tokio::spawn(async move {
        let mut jobs = JoinSet::new();
        loop {
            tokio::select! {
                incoming=listener.accept()=>{let (stream,_)=incoming.unwrap();let log=log.clone();
                    jobs.spawn(async move {
                        if secure {if let Ok(io)=tokio_rustls::TlsAcceptor::from(server_tls()).accept(stream).await {http_session(io,log).await;}}
                        else {http_session(stream,log).await;}
                    });
                }
                Some(result)=jobs.join_next()=>{result.unwrap();}
            }
        }
    });
    Fixture { port, task, seen }
}
async fn http_session(mut io: impl AsyncRead + AsyncWrite + Unpin, seen: Arc<Mutex<Vec<String>>>) {
    let mut first = [0u8; 1];
    if io.read_exact(&mut first).await.is_err() {
        return;
    }
    let mut request = vec![first[0]];
    request.extend(header(&mut io).await);
    let request = String::from_utf8(request).unwrap();
    seen.lock().unwrap().push(request.clone());
    let first = request.lines().next().unwrap();
    if let Some(authority) = first
        .strip_prefix("CONNECT ")
        .and_then(|s| s.strip_suffix(" HTTP/1.1"))
    {
        assert!(request
            .to_ascii_lowercase()
            .contains("proxy-authorization: basic dxnlcjpzzwnyzxq="));
        let mut target = TcpStream::connect(authority).await.unwrap();
        // 分片响应和 informational 必须被正确消费。
        io.write_all(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 Con")
            .await
            .unwrap();
        tokio::task::yield_now().await;
        io.write_all(b"nection Established\r\nContent-Length: 999\r\n\r\n")
            .await
            .unwrap();
        let _ = tokio::io::copy_bidirectional(&mut io, &mut target).await;
    } else {
        let body = "forwarded";
        io.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    }
}
async fn socks_proxy(auth: bool) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let task = tokio::spawn(async move {
        let mut jobs = JoinSet::new();
        loop {
            tokio::select! {
                incoming=listener.accept()=>{let (mut io,_)=incoming.unwrap();let log=log.clone();jobs.spawn(async move {
                    let mut greeting=[0u8;3];if io.read_exact(&mut greeting).await.is_err(){return;}
                    assert_eq!(greeting,[5,1,if auth{2}else{0}]);io.write_all(&[5,greeting[2]]).await.unwrap();
                    if auth {
                        assert_eq!(io.read_u8().await.unwrap(),1);let n=io.read_u8().await.unwrap();let mut user=vec![0;n as usize];io.read_exact(&mut user).await.unwrap();
                        let n=io.read_u8().await.unwrap();let mut password=vec![0;n as usize];io.read_exact(&mut password).await.unwrap();
                        assert_eq!(user,b"u@ser");assert_eq!(password,b"se:cret");io.write_all(&[1,0]).await.unwrap();
                    }
                    let mut h=[0u8;4];io.read_exact(&mut h).await.unwrap();assert_eq!(&h[..3],&[5,1,0]);
                    let host=match h[3] {
                        1=>{let mut ip=[0u8;4];io.read_exact(&mut ip).await.unwrap();std::net::Ipv4Addr::from(ip).to_string()},
                        4=>{let mut ip=[0u8;16];io.read_exact(&mut ip).await.unwrap();format!("[{}]",std::net::Ipv6Addr::from(ip))},
                        3=>{let n=io.read_u8().await.unwrap();let mut bytes=vec![0;n as usize];io.read_exact(&mut bytes).await.unwrap();String::from_utf8(bytes).unwrap()},_=>panic!("invalid address type"),
                    };
                    let port=io.read_u16().await.unwrap();log.lock().unwrap().push(format!("{} {host}",h[3]));
                    let host=if host=="remote-only.invalid"{"127.0.0.1"}else{host.as_str()};
                    let mut target=match TcpStream::connect(format!("{host}:{port}")).await {Ok(io)=>io,Err(_)=>{io.write_all(&[5,5,0,1,0,0,0,0,0,0]).await.unwrap();return;}};
                    io.write_all(&[5,0,0,3,1,b'x',0,0]).await.unwrap();
                    let _=tokio::io::copy_bidirectional(&mut io,&mut target).await;
                });},
                Some(result)=jobs.join_next()=>{result.unwrap();}
            }
        }
    });
    Fixture { port, task, seen }
}
#[test]
fn proxy_example_runs_with_only_network_on_both_runtimes() {
    for multi in [false, true] {
        let runtime = runtime(multi);
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Network).unwrap();
        runtime.block_on(execute(
            &mut machine,
            include_str!("../../examples/proxy.lua"),
        ));
    }
}
#[test]
fn connect_tls_nested_tls_socks_and_websocket_really_tunnel() {
    for multi in [false, true] {
        let runtime = runtime(multi);
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Network).unwrap();
        runtime.block_on(async {
            let http=http_proxy(false).await;let https=http_proxy(true).await;let socks=socks_proxy(false).await;let auth=socks_proxy(true).await;
            execute(&mut machine,&format!(r#"
                local Proxy,Client,Server=cosmos.network.proxy,cosmos.network.http.client,cosmos.network.http.server
                local Ctx=cosmos.network.tcp.tls.context
                local trust=Ctx.new();trust:add_ca_file('{certdir}/ca.pem')
                local server_ctx=Ctx.new_server();server_ctx:use_cert_chain_file('{certdir}/cert.pem');server_ctx:use_private_key_file('{certdir}/key.pem');server_ctx:add_alpn('h2');server_ctx:add_alpn('http/1.1')
                local server=assert(Server.listen('127.0.0.1',0,function(req)
                    assert(req:header('proxy-authorization')==nil,'proxy credentials reached origin')
                    if req:path()=='/ws' then return {{websocket=function(ws) local op,data=ws:receive();assert(op=='text');ws:send('echo:'..data);ws:close() end}} end
                    return {{body=assert(req:body())..' tunneled'}}
                end,{{tls=server_ctx}}))
                local proxies={{
                    assert(Proxy.new('http://user:secret@127.0.0.1:{http}')),
                    assert(Proxy.new('https://user:secret@localhost:{https}',{{ctx=trust}})),
                    assert(Proxy.new('socks5://127.0.0.1:{socks}')),
                    assert(Proxy.new('socks5h://127.0.0.1:{socks}')),
                    assert(Proxy.new('socks5h://u%40ser:se%3Acret@127.0.0.1:{auth}')),
                }}
                for i=1,2 do
                    local forward=Client.new({{proxy=proxies[i]}})
                    assert(forward:request('GET','http://remote-only.invalid/plain').body=='forwarded')
                end
                local bad_proxy=assert(Proxy.new('https://localhost:{https}'))
                local bad_client=Client.new({{proxy=bad_proxy,ctx=trust}})
                local bad_value,bad_err=bad_client:request('GET','http://remote-only.invalid/')
                assert(bad_value==nil and type(bad_err)=='string')
                for _,proxy in ipairs(proxies) do
                    local client=Client.new({{proxy=proxy,ctx=trust}})
                    local response=assert(client:request('POST','https://localhost:'..server:port()..'/echo',{{body='binary\0input',stream=true}}))
                    assert(response.version==20 and response.body:read_all()=='binary\0input tunneled')
                    local ws=assert(cosmos.network.websocket.connect('wss://localhost:'..server:port()..'/ws',{{proxy=proxy,ctx=trust,timeout=2000}}))
                    ws:send('payload');local op,data=ws:receive();assert(op=='text' and data=='echo:payload');ws:close()
                end
                local remote=Client.new({{proxy=proxies[4],ctx=trust}})
                local plain=assert(Server.listen('127.0.0.1',0,function() return 'remote resolved' end))
                assert(remote:request('GET','http://remote-only.invalid:'..plain:port()).body=='remote resolved')
                local no_trust=Client.new({{proxy=proxies[1]}})
                local value,err=no_trust:request('GET','https://localhost:'..server:port());assert(value==nil and type(err)=='string')
                plain:close();server:close()
            "#,http=http.port,https=https.port,socks=socks.port,auth=auth.port,certdir=concat!(env!("CARGO_MANIFEST_DIR"),"/../examples/certs"))).await;
            assert!(http.seen.lock().unwrap().iter().any(|s|s.starts_with("CONNECT localhost:")));
            assert!(socks.seen.lock().unwrap().iter().any(|s|s=="3 remote-only.invalid"));
            assert!(socks.seen.lock().unwrap().iter().any(|s|s.starts_with("1 ")||s.starts_with("4 ")));
        });
    }
}
#[test]
fn malformed_handshakes_timeouts_and_vm_drop_close_proxy_sockets() {
    let runtime = runtime(false);
    runtime.block_on(async {
        // 每个脚本连接独立代理，损坏响应不能成为可复用连接。
        for (scheme,reply) in [
            ("http",b"HTTP/1.1 407 No\r\n\r\n".as_slice()),
            ("http",b"invalid\r\n\r\n".as_slice()),
            ("http",b"HTTP/1.1 200 OK\r\nx: ".as_slice()),
            ("socks5h",b"\x05\xff".as_slice()),
            ("socks5h",b"\x04\x00".as_slice()),
        ] {
            let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let port=listener.local_addr().unwrap().port();
            let response=reply.to_vec();let task=tokio::spawn(async move {
                let (mut io,_)=listener.accept().await.unwrap();
                if scheme=="http" {header(&mut io).await;}else{io.read_exact(&mut [0u8;3]).await.unwrap();}
                io.write_all(&response).await.unwrap();
            });
            let mut machine=Machine::new(tokio::runtime::Handle::current()).unwrap();machine.add_module(Network).unwrap();
            execute(&mut machine,&format!(r#"
                local proxy=assert(cosmos.network.proxy.new('{scheme}://user:secret@127.0.0.1:{port}'))
                local client=cosmos.network.http.client.new({{proxy=proxy}})
                local result,err=client:request('GET','https://remote-only.invalid/',{{timeout=1000}})
                assert(result==nil and type(err)=='string' and not err:find('secret',1,true))
            "#)).await;task.await.unwrap();
        }
        // 握手完成前超时和 Machine 销毁必须关闭底层 TCP。
        for drop_vm in [false,true] {
            let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let port=listener.local_addr().unwrap().port();
            let (ready,started)=tokio::sync::oneshot::channel();
            let task=tokio::spawn(async move {
                let (mut io,_)=listener.accept().await.unwrap();header(&mut io).await;ready.send(()).unwrap();
                let n=tokio::time::timeout(Duration::from_secs(2),io.read(&mut [0u8;1])).await.unwrap().unwrap();
                assert_eq!(n,0,"cancelled proxy handshake retained the connection");
            });
            let mut machine=Machine::new(tokio::runtime::Handle::current()).unwrap();machine.add_module(Network).unwrap();
            let code=format!(r#"
                local p=assert(cosmos.network.proxy.new('http://127.0.0.1:{port}'))
                client=cosmos.network.http.client.new({{proxy=p,connect_timeout=1000}})
                local function request() local value,err=client:request('GET','https://remote-only.invalid/',{{timeout=30}});assert(value==nil and err=='timeout') end
                {action}
            "#,action=if drop_vm{"cosmos.thread.spawn(request);cosmos.time.sleep(10)"}else{"request()"});
            execute(&mut machine,&code).await;started.await.unwrap();drop(machine);task.await.unwrap();
        }
    });
}
