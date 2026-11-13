use std::cell::{Cell, RefCell};
use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::thread::{self, ThreadId};

use cosmos::userdata::LuaFunction;
use cosmos::{Machine, Module};
use cosmos_network::Network;
use tokio::runtime::{Builder, Runtime};

thread_local! {
    static HOST_THREAD: Cell<Option<ThreadId>> = const { Cell::new(None) };
    static SAVED: RefCell<Option<LuaFunction>> = const { RefCell::new(None) };
    static DROPPED: Cell<usize> = const { Cell::new(0) };
    static MODULE_TASK_READY: Cell<bool> = const { Cell::new(false) };
}

struct Probe;

#[cosmos::userdata(name = "test.probe")]
impl Probe {
    fn on_host_thread() -> bool {
        HOST_THREAD.with(|owner| owner.get() == Some(thread::current().id()))
    }

    fn module_task_ready() -> bool {
        MODULE_TASK_READY.with(Cell::get)
    }

    fn save(callback: LuaFunction) {
        SAVED.with(|saved| *saved.borrow_mut() = Some(callback));
    }

    fn new() -> Self {
        Self
    }

    async fn wait(&mut self) {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }

    fn touch(&mut self) -> bool {
        true
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        DROPPED.with(|count| count.set(count.get() + 1));
    }
}

struct ProbeModule;

impl Module for ProbeModule {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        assert!(tokio::runtime::Handle::try_current().is_ok());
        HOST_THREAD.with(|owner| owner.set(Some(thread::current().id())));
        MODULE_TASK_READY.with(|ready| ready.set(false));
        drop(tokio::task::spawn_local(async {
            MODULE_TASK_READY.with(|ready| ready.set(true));
        }));
        machine.register::<Probe>();
        machine.load("测试插件", "plugin_ready = true")
    }
}

fn runtime(multithread: bool) -> Runtime {
    let mut builder = if multithread {
        Builder::new_multi_thread()
    } else {
        Builder::new_current_thread()
    };
    builder.worker_threads(2).enable_all().build().unwrap()
}

#[test]
fn injected_runtimes_keep_lua_on_the_host_thread_and_plugins_are_optional() {
    for multithread in [false, true] {
        let runtime = runtime(multithread);
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        runtime
            .block_on(machine.exec("assert(cosmos.network == nil); assert(cosmos.time ~= nil)"))
            .unwrap();
        machine.add_module(Network).unwrap();
        machine.add_module(ProbeModule).unwrap();
        runtime
            .block_on(machine.exec(
                r#"
            assert(plugin_ready)
            cosmos.time.sleep(0)
            assert(cosmos.test.probe.module_task_ready())
            assert(cosmos.test.probe.on_host_thread())
            local workers = {}
            for i = 1, 4 do
                workers[i] = cosmos.thread.spawn(function()
                    for j = 1, 3 do
                        cosmos.time.sleep(1)
                        assert(cosmos.test.probe.on_host_thread())
                    end
                end)
            end
            for _, worker in ipairs(workers) do
                local _, err = worker:join()
                assert(err == nil, err)
            end
            local Server = require("cosmos.network.http.server")
            local Client = require("cosmos.network.http.client")
            local server = assert(Server.listen("127.0.0.1", 0, function()
                assert(cosmos.test.probe.on_host_thread())
                return "ok"
            end))
            local reply = assert(Client.new():request("GET", "http://127.0.0.1:" .. server:port() .. "/"))
            assert(reply.body == "ok")
            server:close()
        "#,
            ))
            .unwrap();
        drop(machine);
        // Machine 的销毁不能关闭注入的 Runtime。
        runtime.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(1)).await });
    }
}

#[test]
fn machines_are_isolated_and_closing_one_does_not_leak_anothers_lua_values() {
    let runtime = runtime(false);
    let mut first = Machine::new(runtime.handle().clone()).unwrap();
    let mut second = Machine::new(runtime.handle().clone()).unwrap();
    runtime.block_on(first.exec("value = 1")).unwrap();
    runtime
        .block_on(second.exec(
            r#"
        assert(value == nil)
        value = 2
        queue = cosmos.sync.channel.new(1)
        weak = setmetatable({}, { __mode = "v" })
        local function put()
            local object = {}
            weak[1] = object
            assert(queue:send(object))
        end
        put()
    "#,
        ))
        .unwrap();
    drop(first);
    runtime
        .block_on(second.exec(
            r#"
        assert(value == 2)
        local function take() assert(queue:recv() == weak[1]) end
        take()
        collectgarbage("collect")
        assert(weak[1] == nil, "关闭另一个 VM 后 registry 引用泄漏")
    "#,
        ))
        .unwrap();
}

#[test]
fn execution_recovers_after_errors_and_background_tasks_survive_between_calls() {
    let runtime = runtime(false);
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(ProbeModule).unwrap();
    assert!(runtime.block_on(machine.exec("local = invalid")).is_err());
    assert!(runtime
        .block_on(machine.exec("error('expected')"))
        .unwrap_err()
        .contains("expected"));
    assert!(runtime
        .block_on(machine.exec("--\0\nerror('buffer-loaded')"))
        .unwrap_err()
        .contains("buffer-loaded"));
    assert!(machine
        .load("bad-module", "cosmos.time.sleep(10000)")
        .is_err());
    machine.load("recovery", "counter = 40").unwrap();
    runtime
        .block_on(machine.exec(
            r#"
        cosmos.thread.spawn(function()
            cosmos.time.sleep(2)
            counter = counter + 2
        end)
        cosmos.time.sleep(0)
    "#,
        ))
        .unwrap();
    runtime
        .block_on(machine.exec("cosmos.time.sleep(10); assert(counter == 42)"))
        .unwrap();
    runtime
        .block_on(machine.exec_with_args(
            "=args",
            "local a,b=...; assert(a=='中文' and b=='a\\0b')",
            &["中文".into(), "a\0b".into()],
        ))
        .unwrap();
    runtime.block_on(async {
        let waiting = machine.exec("waiting = cosmos.test.probe.new(); waiting:wait()");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(2), waiting)
                .await
                .is_err()
        );
        machine
            .exec("assert(waiting:touch()); waiting = nil; collectgarbage('collect')")
            .await
            .unwrap();
    });
}

#[test]
fn wrong_or_missing_runtime_is_reported_without_running_lua() {
    let first = runtime(false);
    let second = runtime(false);
    let mut machine = Machine::new(first.handle().clone()).unwrap();
    assert!(second.block_on(machine.exec("ran = true")).is_err());
    {
        let mut execution = Box::pin(machine.exec("ran = true"));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            execution.as_mut().poll(&mut context),
            Poll::Ready(Err(_))
        ));
    }
    first.block_on(machine.exec("assert(ran == nil)")).unwrap();
}

#[test]
fn http_pools_do_not_reuse_a_paused_runtime_connection() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // 服务端独立运行；两个客户端 runtime 在同一宿主线程先后运行，旧 runtime 保留但暂停驱动。
    let server_runtime = runtime(true);
    let listener = server_runtime
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = server_runtime.spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                loop {
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    if reader
                        .get_mut()
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    let mut runtimes = Vec::new();
    for _ in 0..2 {
        let runtime = runtime(false);
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Network).unwrap();
        runtime.block_on(machine.exec(&format!(r#"
            local Client = require("cosmos.network.http.client")
            local reply = assert(Client.new():request("GET", "http://127.0.0.1:{port}/", {{ timeout = 250 }}))
            assert(reply.body == "ok")
        "#))).unwrap();
        drop(machine);
        runtimes.push(runtime);
    }
    server.abort();
}

#[test]
fn collecting_a_client_releases_its_idle_connection() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let runtime = runtime(false);
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = runtime.spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(socket);
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
        }
        reader
            .get_mut()
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
        // 服务端保持 keep-alive，只有 client 的池释放后才会读到 EOF。
        assert_eq!(reader.read(&mut [0]).await.unwrap(), 0);
    });
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    runtime.block_on(async {
        machine
            .exec(&format!(
                r#"
            local Client = require("cosmos.network.http.client")
            local function request_once()
                local client = Client.new()
                assert(client:request("GET", "http://127.0.0.1:{port}/").body == "ok")
            end
            request_once()
            collectgarbage("collect")
            cosmos.time.sleep(0)
        "#
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    });
}

#[test]
fn drop_cancels_borrowing_tasks_before_vm_finalizers_and_old_callbacks_stay_closed() {
    let runtime = runtime(false);
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(ProbeModule).unwrap();
    DROPPED.with(|count| count.set(0));
    runtime
        .block_on(machine.exec(
            r#"
        local probe = cosmos.test.probe.new()
        cosmos.test.probe.save(function() return 7 end)
        cosmos.thread.spawn(function() probe:wait() end)
        cosmos.time.sleep(0)
    "#,
        ))
        .unwrap();
    let callback = SAVED.with(|saved| saved.borrow_mut().take().unwrap());
    drop(machine);
    DROPPED.with(|count| assert_eq!(count.get(), 1));
    let mut another = Machine::new(runtime.handle().clone()).unwrap();
    assert!(runtime
        .block_on(callback.call::<_, ()>(()))
        .unwrap_err()
        .contains("已关闭"));
    drop(callback);
    runtime
        .block_on(another.exec("cosmos.time.sleep(0)"))
        .unwrap();
}
