//! 插件在独立 Machine 中注册并运行真实 Lua 用例。
use cosmos::Machine;
use cosmos_encoding::Encoding;

#[test]
fn codecs_work_with_binary_data_and_are_installed_per_machine() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut first = Machine::new(runtime.handle().clone()).unwrap();
    let mut second = Machine::new(runtime.handle().clone()).unwrap();
    first.add_module(Encoding).unwrap();
    runtime
        .block_on(first.exec(include_str!("../../examples/encoding.lua")))
        .unwrap();
    runtime
        .block_on(second.exec("assert(cosmos.encoding == nil)"))
        .unwrap();
}
