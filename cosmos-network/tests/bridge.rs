//! 网络叶子转换与带状态闭包的跨 crate 回归，不依赖原始 Lua API。

use cosmos::Machine;
use cosmos_network::Network;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn tcp_iterator_survives_binary_data_timeout_and_cancelled_calls() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = runtime.spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        let mut payload = vec![b'x'; 8192];
        payload.extend_from_slice(b"\0\xfffirst||second||");
        writer.write_all(&payload).await.unwrap();
        let mut signal = [0];
        reader.read_exact(&mut signal).await.unwrap();
        assert_eq!(signal, *b"N");
        writer.write_all(b"after||tail").await.unwrap();
        writer.shutdown().await.unwrap();
    });
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            machine.exec(&format!(
                r#"
            local Tcp = require("cosmos.network.tcp.stream")
            local conn = assert(Tcp.connect("127.0.0.1", {port}, 1000))
            local reader, writer = conn:split()
            local repeated, split_error = conn:split()
            assert(repeated == nil and split_error:find("already split", 1, true))
            reader:settimeout(100)
            local iter = assert(reader:receiveuntil("||"))
            assert(iter() == string.rep("x", 8192)..string.char(0, 255).."first")
            assert(iter() == "second")
            local value, err, partial = iter()
            assert(value == nil and err == "timeout" and partial == "")

            local started = false
            local waiting = cosmos.thread.spawn(function()
                started = true
                iter()
                error("cancelled iterator calls must not resume")
            end)
            while not started do cosmos.time.sleep(0) end
            waiting:kill()
            local _, cancelled = waiting:join()
            assert(cancelled:find("cancelled", 1, true))
            collectgarbage("collect")
            assert(writer:write("N") == 1)
            assert(iter() == "after", "cancellation must release the exclusive iterator borrow")
            local tail, closed, rest = iter()
            assert(tail == nil and closed == "closed" and rest == "tail")
            writer:close()
        "#
            )),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
    });
}
