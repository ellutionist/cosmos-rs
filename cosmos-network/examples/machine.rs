//! Rust 宿主示例：注入多线程 Runtime Handle，并安装网络和自定义插件。

use cosmos::{Machine, Module};
use cosmos_network::Network;

struct Host;

#[cosmos::userdata(name = "example.host")]
impl Host {
    fn add(a: i64, b: i64) -> i64 {
        a + b
    }
}

struct HostModule;

impl Module for HostModule {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        machine.register::<Host>();
        machine.load("host definitions", "host_ready = true")
    }
}

fn main() -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let mut machine = Machine::new(runtime.handle().clone())?;
    machine.add_module(Network)?;
    machine.add_module(HostModule)?;

    runtime.block_on(async {
        machine
            .exec("assert(host_ready); answer = cosmos.example.host.add(20, 22)")
            .await?;
        machine
            .exec("cosmos.time.sleep(1); assert(answer == 42)")
            .await?;
        machine
            .exec_file(
                concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/machine.lua"),
                &["host".to_string(), "\u{53c2}\u{6570}".to_string()],
            )
            .await
    })?;
    drop(machine);
    println!("Machine embedding example passed");
    Ok(())
}
