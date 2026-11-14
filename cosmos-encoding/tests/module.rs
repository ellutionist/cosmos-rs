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

#[test]
fn json_preserves_shapes_null_and_numbers_and_recovers_after_errors() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Encoding).unwrap();
    runtime
        .block_on(machine.exec(include_str!("../../examples/json.lua")))
        .unwrap();
    // 重复安装不能使之前解码的容器或精确数字失效。
    runtime.block_on(machine.exec("saved_json_array = cosmos.encoding.json.decode('[]'); saved_json_number = cosmos.encoding.json.decode('18446744073709551615')")).unwrap();
    machine.add_module(Encoding).unwrap();
    runtime.block_on(machine.exec("assert(cosmos.encoding.json.encode(saved_json_array) == '[]'); assert(cosmos.encoding.json.encode(saved_json_number) == '18446744073709551615')")).unwrap();
}
