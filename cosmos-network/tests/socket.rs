//! 顺序协议 socket 的取消与缓冲回归。

use cosmos::Machine;
use cosmos_network::Network;
use tokio::io::AsyncReadExt;

#[test]
fn socket_example_covers_upgrade_unix_and_failed_reads() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    // 脚本中的证书路径以项目根目录为基准，避免修改进程工作目录。
    let source = include_str!("../../examples/network_socket.lua").replace(
        "examples/certs/",
        &format!("{}/../examples/certs/", env!("CARGO_MANIFEST_DIR")),
    );
    runtime.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(10), machine.exec(&source))
            .await
            .unwrap()
            .unwrap();
    });
}

#[test]
fn cancelling_a_partial_write_discards_the_connection() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (release, hold) = tokio::sync::oneshot::channel();
    let server = runtime.spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        hold.await.unwrap();
        let mut bytes = 0;
        let mut buffer = [0; 8192];
        loop {
            let count = stream.read(&mut buffer).await.unwrap();
            if count == 0 {
                break;
            }
            bytes += count;
        }
        bytes
    });
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    runtime.block_on(async {
        machine
            .exec(&format!(
                r#"
            local socket = cosmos.network.socket.new()
            assert(socket:connect("127.0.0.1", {port}))
            local started = false
            local sender = cosmos.thread.spawn(function()
                started = true
                socket:send(string.rep("x", 16 * 1024 * 1024))
                error("写入不应在服务端停止读取时完成")
            end)
            while not started do cosmos.time.sleep(0) end
            sender:kill()
            local _, err = sender:join()
            assert(err and err:find("cancelled", 1, true))
            assert(socket:state() == 1)
            local value, closed = socket:send("after")
            assert(value == nil and closed == "closed")
        "#
            ))
            .await
            .unwrap();
        release.send(()).unwrap();
        let sent = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert!(sent > 0 && sent < 16 * 1024 * 1024);
    });
}
