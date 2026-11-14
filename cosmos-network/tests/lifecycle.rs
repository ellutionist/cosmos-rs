//! HTTP 排空在两种注入 Runtime 验证；主动取消不丢弃服务端清理状态。
use cosmos::Machine;
use cosmos_network::Network;
#[test]
fn graceful_shutdown_drains_http1_http2_and_forces_stalled_requests() {
    for multi in [false, true] {
        let mut builder = if multi {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = builder.worker_threads(2).enable_all().build().unwrap();
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Network).unwrap();
        let source = format!(
            "local function run(...)\n{}\nend\nrun({:?})",
            include_str!("../../examples/http_shutdown.lua"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/certs")
        );
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(15), machine.exec(&source))
                .await
                .unwrap()
                .unwrap();
        });
    }
}
#[test]
fn cancelled_shutdown_waiters_can_resume_and_tls_handshakes_are_closed() {
    use tokio::io::AsyncReadExt;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    runtime.block_on(async {
        let certs=concat!(env!("CARGO_MANIFEST_DIR"),"/../examples/certs");
        machine.exec(&format!(r#"
            local tls=cosmos.network.tcp.tls.context.new_server()
            tls:use_cert_chain_file('{certs}/cert.pem');tls:use_private_key_file('{certs}/key.pem')
            server=assert(cosmos.network.http.server.listen('127.0.0.1',0,function() error('unnegotiated TLS reached handler') end,{{tls=tls}}))
            server_port=server:port()
        "#)).await.unwrap();
        // 返回端口通过独立 Lua 执行值传出，保持 Machine 宿主执行 API 的现有形态。
        let scratch=tempfile::NamedTempFile::new().unwrap();
        machine.exec(&format!("assert(cosmos.fs.write_file({:?},tostring(server_port)))",scratch.path().to_str().unwrap())).await.unwrap();
        let port=std::fs::read_to_string(scratch.path()).unwrap().parse::<u16>().unwrap();
        let mut peer=tokio::net::TcpStream::connect(("127.0.0.1",port)).await.unwrap();
        machine.exec("cosmos.time.sleep(1);assert(server:connections()==1);assert(server:shutdown(100));assert(server:connections()==0)").await.unwrap();
        assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(1),peer.read(&mut [0u8;1])).await.unwrap().unwrap(),0);
        machine.exec(r#"
            local channel=cosmos.sync.channel.new(1)
            local server=assert(cosmos.network.http.server.listen('127.0.0.1',0,function() channel:send(true);cosmos.time.sleep(20);return 'done' end))
            local request=cosmos.thread.spawn(function() assert(cosmos.network.http.client.new():request('GET','http://127.0.0.1:'..server:port()).body=='done') end)
            assert(channel:recv())
            local waiting=cosmos.thread.spawn(function() server:shutdown() end)
            cosmos.time.sleep(1);waiting:kill();waiting:join()
            assert(server:is_draining());assert(server:shutdown(1000));assert(request:join()==nil)
        "#).await.unwrap();
    });
}
