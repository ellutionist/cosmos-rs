//! 不依赖其他插件；文本输出、调用位置、宿主 subscriber 和异步写入。
use cosmos::Machine;
use cosmos_logging::Logging;
#[test]
fn logging_example_works_without_other_plugins_on_both_runtimes() {
    for multi in [false, true] {
        let mut builder = if multi {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = builder.worker_threads(2).enable_all().build().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        runtime
            .block_on(machine.exec("assert(cosmos.log==nil)"))
            .unwrap();
        machine.add_module(Logging).unwrap();
        machine.add_module(Logging).unwrap();
        let source=format!("assert(cosmos.network==nil and cosmos.encoding==nil)\nlocal function run(...)\n{}\nend\nrun({:?})",include_str!("../../examples/logging.lua"),scratch.path().to_str().unwrap());
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), machine.exec(&source))
                .await
                .unwrap()
                .unwrap();
        });
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }
}
#[derive(Clone)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
struct Visitor(String);
impl tracing::field::Visit for Visitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={value:?};", field.name()));
    }
}
impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target() == "cosmos.lua" {
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
#[test]
fn host_output_keeps_the_scoped_subscriber_and_lua_location() {
    let capture = Capture(Default::default());
    let events = capture.0.clone();
    tracing::subscriber::with_default(capture, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Logging).unwrap();
        runtime.block_on(machine.exec_with_args("@host_logging.lua","local log=assert(cosmos.log.new({console='host'}))\nassert(log:info('host attached',{count=7}))\nassert(log:close())",&[])).unwrap();
    });
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(
        events[0].contains("host_logging.lua:2 host attached"),
        "{}",
        events[0]
    );
    assert!(events[0].contains("lua_line=2"));
    assert!(events[0].contains("count=7"));
}

// 阻塞原生输出，确定性验证写入预算与 Machine 销毁；工作线程只接收拥有型记录。
#[derive(Clone)]
struct BlockingCapture {
    capture: Capture,
    started: std::sync::Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<()>>>>,
    gate: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}
impl tracing::Subscriber for BlockingCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target() != "cosmos.lua" {
            return;
        }
        if let Some(started) = self.started.lock().unwrap().take() {
            started.send(()).unwrap();
            let (lock, ready) = &*self.gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = ready.wait(released).unwrap();
            }
        }
        self.capture.event(event);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
#[test]
fn bounded_writes_release_lua_before_a_native_job_finishes() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    struct Release(std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0 .0.lock().unwrap() = true;
            self.0 .1.notify_all();
        }
    }
    let release = Release(gate.clone());
    let (started, receiver) = std::sync::mpsc::sync_channel(1);
    let capture = Capture(Default::default());
    let events = capture.0.clone();
    tracing::subscriber::with_default(
        BlockingCapture {
            capture,
            started: std::sync::Arc::new(std::sync::Mutex::new(Some(started))),
            gate,
        },
        || {
            let mut machine = Machine::new(runtime.handle().clone()).unwrap();
            machine.add_module(Logging).unwrap();
            runtime.block_on(machine.exec_with_args("@bounded_logging.lua",r#"local logger=assert(cosmos.log.new({console='host'}))
local rejected=0
for i=1,200 do cosmos.thread.spawn(function() local value,err=logger:info('queued');if value==nil then assert(err=='log queue is full',err);rejected=rejected+1 end end) end
cosmos.time.sleep(20)
assert(rejected==72,'log admission budget is not bounded')"#,&[])).unwrap();
            receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            drop(machine);
        },
    );
    drop(release);
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].contains("bounded_logging.lua:3 queued"));
}
