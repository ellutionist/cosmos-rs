//! Image 插件独立注入、两种 Runtime 与实际文件操作。
use cosmos::Machine;
use cosmos_image::Image;
use std::time::Duration;

#[test]
fn images_work_without_other_plugins_on_both_injected_runtimes() {
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
            .block_on(
                machine.exec("assert(cosmos.image==nil and not pcall(require,'cosmos.image'))"),
            )
            .unwrap();
        machine.add_module(Image).unwrap();
        machine.add_module(Image).unwrap();
        let source=format!("assert(cosmos.encoding==nil and cosmos.network==nil)\nlocal function run(...)\n{}\nend\nrun({:?})",include_str!("../../examples/image.lua"),scratch.path().to_str().unwrap());
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), machine.exec(&source))
                .await
                .unwrap()
                .unwrap();
        });
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }
}
#[test]
fn cancellation_and_machine_drop_do_not_keep_lua_alive() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for _ in 0..2 {
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Image).unwrap();
        runtime.block_on(async{tokio::time::timeout(Duration::from_secs(30),machine.exec(r#"
            local Image=cosmos.image
            local image=assert(Image.new(2048,1536,{r=50,g=100,b=150}))
            local weak=setmetatable({},{__mode='v'})
            local function start()
                local owner={image=image};weak[1]=owner
                return cosmos.thread.spawn(function() owner.image:resize(1024,768,'lanczos3') end)
            end
            local task=start();cosmos.time.sleep(1);task:kill()
            cosmos.time.sleep(1);collectgarbage('collect');collectgarbage('collect')
            assert(weak[1]==nil,'cancelled image worker retained Lua references')
            local ticks,running=0,true
            local ticker=cosmos.thread.spawn(function()
                while running do cosmos.time.sleep(1);if running then ticks=ticks+1 end end
            end)
            local resized=assert(image:resize(1024,768,'triangle'))
            running=false;assert(ticker:join()==nil and ticks>0,'image worker blocked the host thread')
            assert(resized:width()==1024 and image:width()==2048)
            cosmos.thread.spawn(function() image:resize(1200,900,'lanczos3') end)
            cosmos.time.sleep(1)
        "#)).await.unwrap().unwrap();});
        drop(machine);
    }
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Image).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(30),
            machine.exec("assert(cosmos.image.new(1,1):width()==1)"),
        )
        .await
        .unwrap()
        .unwrap();
    });
}

#[test]
fn image_processing_composes_with_streaming_http() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Image).unwrap();
    machine.add_module(cosmos_network::Network).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(10),
            machine.exec(include_str!("../../examples/image_http.lua")),
        )
        .await
        .unwrap()
        .unwrap();
    });
}
