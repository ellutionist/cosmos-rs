//! WebSocket 并发收发与取消不破坏连接。
use std::time::Duration;

use cosmos::Machine;
use cosmos_network::Network;

#[test]
fn websocket_supports_full_duplex_and_cancel_safe_receives() {
    for multithread in [false, true] {
        let mut builder = if multithread {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = builder.worker_threads(2).enable_all().build().unwrap();
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Network).unwrap();
        runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(10),
                machine.exec(include_str!("../../examples/websocket_concurrent.lua")),
            )
            .await
            .unwrap()
            .unwrap();
        });
    }
}

#[test]
fn partial_frames_survive_receive_timeout_and_vm_drop_releases_transport() {
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = runtime.spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (finished, mut finish) = tokio::sync::mpsc::channel(1);
        let service = hyper::service::service_fn(move |mut request| {
            let finished = finished.clone();
            async move {
                let (response, upgrade) = fastwebsockets::upgrade::upgrade(&mut request).unwrap();
                tokio::spawn(async move {
                    let mut io = upgrade.await.unwrap().into_inner();
                    // 第一片完整，续片头只发一个字节；接收超时不能重置帧解析状态。
                    io.write_all(&[0x01, 3, b'o', b'n', b'e', 0x80])
                        .await
                        .unwrap();
                    let mut command = [0; 14];
                    io.read_exact(&mut command).await.unwrap();
                    io.write_all(&[3, b't', b'w', b'o']).await.unwrap();
                    let mut tail = [0; 16];
                    assert_eq!(io.read(&mut tail).await.unwrap(), 0);
                    finished.send(()).await.unwrap();
                });
                Ok::<_, Infallible>(response)
            }
        });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(socket), service)
            .with_upgrades()
            .await
            .unwrap();
        finish.recv().await.unwrap();
    });
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(3),
            machine.exec(&format!(
                r#"
            local conn = assert(cosmos.network.websocket.connect('ws://127.0.0.1:{port}/'))
            conn:settimeout(10)
            local value, err = conn:receive()
            assert(value == nil and err == 'timeout', err)
            conn:settimeout(0)
            local reader = cosmos.thread.spawn(function() conn:receive() end)
            cosmos.time.sleep(5)
            reader:kill()
            local _, write_err = conn:send('continue')
            assert(write_err == nil, write_err)
            local kind, message = conn:receive()
            assert(kind == 'text' and message == 'onetwo', message)
            cosmos.thread.spawn(function() conn:receive() end)
            cosmos.time.sleep(5)
            collectgarbage('collect')
        "#
            )),
        )
        .await
        .unwrap()
        .unwrap();
    });
    drop(machine);
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    });
}
