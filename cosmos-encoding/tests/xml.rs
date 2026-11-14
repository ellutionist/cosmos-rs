//! XML 文档、原文编辑与异步/生命周期验证。
use cosmos::Machine;
use cosmos_encoding::Encoding;
use std::time::Duration;

#[test]
fn xml_supports_generic_documents_and_precise_edits_on_both_runtimes() {
    for multi in [false, true] {
        let mut builder = if multi {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = builder.worker_threads(2).enable_all().build().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Encoding).unwrap();
        machine.add_module(Encoding).unwrap();
        let source = format!(
            "local function run(...)\n{}\nend\nrun({:?})",
            include_str!("../../examples/xml.lua"),
            scratch.path().to_str().unwrap()
        );
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(20), machine.exec(&source))
                .await
                .unwrap()
                .unwrap();
        });
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }
}

#[test]
fn xml_cancellation_and_machine_drop_release_lua_and_allow_recovery() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for _ in 0..2 {
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Encoding).unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10),machine.exec(r#"
                local xml=cosmos.encoding.xml
                local weak=setmetatable({},{__mode='v'})
                local huge='<r>'..string.rep('<c key="value">some text</c>',40000)..'</r>'
                local function start()
                    local owner={input=huge}; weak[1]=owner
                    return cosmos.thread.spawn(function() xml.parse(owner.input) end)
                end
                local task=start(); cosmos.time.sleep(1); task:kill()
                cosmos.time.sleep(1); collectgarbage('collect'); collectgarbage('collect')
                assert(weak[1]==nil,'cancelled XML job retained Lua references')
                local ticks,running=0,true
                local ticker=cosmos.thread.spawn(function()
                    while running do cosmos.time.sleep(1); if running then ticks=ticks+1 end end
                end)
                local document=assert(xml.parse(huge))
                running=false; assert(ticker:join()==nil and ticks>0,'XML parsing blocked the host thread')
                saved_node=document:root():child('c')
                document=nil; collectgarbage('collect')
                assert(saved_node:attribute('key')=='value' and saved_node:text()=='some text')
                cosmos.thread.spawn(function() xml.parse(huge) end)
                cosmos.time.sleep(1)
            "#)).await.unwrap().unwrap();
        });
        drop(machine);
    }
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Encoding).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(10),
            machine.exec(
                "assert(cosmos.encoding.xml.parse('<x>recovered</x>'):root():text()=='recovered')",
            ),
        )
        .await
        .unwrap()
        .unwrap();
    });
}
