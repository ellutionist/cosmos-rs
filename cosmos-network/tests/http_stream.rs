//! 真实网络上的文件传输、Lua 回调线程边界与 VM 销毁回归。
use cosmos::{Machine, Module};
use cosmos_network::Network;
use std::cell::Cell;
use std::thread::{self, ThreadId};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::{Builder, Runtime};

fn runtime(multithread: bool) -> Runtime {
    let mut builder = if multithread {
        Builder::new_multi_thread()
    } else {
        Builder::new_current_thread()
    };
    builder.worker_threads(2).enable_all().build().unwrap()
}

#[test]
fn streaming_files_ranges_timeouts_and_backpressure_on_injected_runtimes() {
    for multithread in [false, true] {
        let runtime = runtime(multithread);
        let scratch = tempfile::tempdir().unwrap();
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Network).unwrap();
        let source = format!(
            "local function run(...)\n{}\nend\nrun({:?}, {:?})",
            include_str!("../../examples/http_stream.lua"),
            scratch.path().to_str().unwrap(),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/certs"),
        );
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(60), machine.exec(&source))
                .await
                .unwrap()
                .unwrap();
        });
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }
}

thread_local! {
    static HOST: Cell<Option<ThreadId>> = const { Cell::new(None) };
}
struct Probe;
#[cosmos::userdata(name = "test.stream_probe")]
impl Probe {
    fn on_host_thread() -> bool {
        HOST.with(|owner| owner.get() == Some(thread::current().id()))
    }
}
struct ProbeModule;
impl Module for ProbeModule {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        HOST.with(|owner| owner.set(Some(thread::current().id())));
        machine.register::<Probe>();
        Ok(())
    }
}

#[test]
fn callbacks_and_concurrent_reads_stay_local_and_cancellation_closes_the_source() {
    let runtime = runtime(true);
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    machine.add_module(ProbeModule).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(5),
            machine.exec(
                r#"
            local body = cosmos.network.http.body
            local function check() assert(cosmos.test.stream_probe.on_host_thread()) end
            local function source()
                local remaining = 20
                return body.from_callback(function()
                    check(); cosmos.time.sleep(1); check()
                    if remaining == 0 then return nil end
                    remaining = remaining - 1
                    return 'x'
                end)
            end
            local srv = assert(cosmos.network.http.server.listen('127.0.0.1', 0, function(req)
                check()
                assert(req:body() == string.rep('x',20))
                return {body=source()}
            end, {stream_body=true}))
            local resp = assert(cosmos.network.http.client.new():request('POST',
                'http://127.0.0.1:' .. srv:port(), {body=source(), stream=true}))
            assert(resp.body:read_all() == string.rep('x',20))
            srv:close()
            local entered = cosmos.sync.channel.new(1)
            local b = body.from_callback(function()
                entered:send(true); cosmos.time.sleep(10000); return 'never'
            end)
            local active = cosmos.thread.spawn(function() b:read() end)
            assert(entered:recv())
            local queued = cosmos.thread.spawn(function()
                local value, err = b:read(); assert(value == nil and err == 'closed',err)
            end)
            cosmos.time.sleep(1); active:kill()
            assert(queued:join() == nil and b:is_closed())
            local b2 = body.from_callback(function()
                cosmos.time.sleep(10000); return 'never'
            end)
            local readers = {}
            for i=1,3 do readers[i] = cosmos.thread.spawn(function()
                local value, err = b2:read(); assert(value == nil and err == 'closed',err)
            end) end
            cosmos.time.sleep(1); b2:close()
            for _, reader in ipairs(readers) do assert(reader:join() == nil) end
        "#,
            ),
        )
        .await
        .unwrap()
        .unwrap();
    });
}

#[test]
fn vm_drop_closes_inflight_upload_and_unread_response() {
    for multithread in [false, true] {
        let runtime = runtime(multithread);
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = runtime.spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            let mut buffer = [0; 4096];
            while !header.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                header.extend_from_slice(&buffer[..count]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n")
                .await
                .unwrap();
            loop {
                match socket.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(_) => (),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        break
                    }
                    Err(error) => panic!("peer read failed: {error}"),
                }
            }
        });
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Network).unwrap();
        runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(3),
                machine.exec(&format!(
                    r#"
                upload = cosmos.network.http.body.from_callback(function()
                    cosmos.time.sleep(10000); return 'pending'
                end)
                response = assert(cosmos.network.http.client.new():request('POST',
                    'http://127.0.0.1:{port}/', {{body=upload, stream=true}}))
                assert(response.body:read(1) == 'x')
            "#
                )),
            )
            .await
            .unwrap()
            .unwrap();
        });
        drop(machine);
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), peer)
                .await
                .unwrap()
                .unwrap();
        });
    }
}

#[test]
fn incoming_trailers_and_truncated_content_length() {
    let runtime = runtime(false);
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = runtime.spawn(async move {
        for response in [
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: x-sum\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\nx-sum: test\r\n\r\n"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc"[..],
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
            }
            socket.write_all(response).await.unwrap();
        }
    });
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(3),
            machine.exec(&format!(
                r#"
            local client = cosmos.network.http.client.new()
            local first = assert(client:request('GET','http://127.0.0.1:{port}/',{{stream=true}}))
            assert(first.body:trailers() == nil)
            assert(first.body:read_all() == 'abc')
            assert(first.body:trailers()['x-sum'] == 'test')
            local second = assert(client:request('GET','http://127.0.0.1:{port}/',{{stream=true}}))
            local data, err = second.body:read_all()
            assert(data == nil and err and second.body:is_closed(),err)
        "#
            )),
        )
        .await
        .unwrap()
        .unwrap();
        peer.await.unwrap();
    });
}
