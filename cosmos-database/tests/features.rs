//! 单独运行本 crate 的 feature 组合，防止工作区 feature 合并掩盖边界。
use cosmos::Machine;
use cosmos_database::Database;

#[test]
fn database_features_control_registration_and_dependencies() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Database).unwrap();
    let source = format!(
        r#"
        for name, enabled in pairs({{mysql={mysql}, redis={redis}}}) do
            local ok, value = pcall(require, 'cosmos.database.' .. name)
            assert(ok == enabled)
            if enabled then assert(value == cosmos.database[name]) end
        end
        if not {mysql} then assert(cosmos.encoding == nil and cosmos.encryption == nil) end
        if not {mysql} and not {redis} then assert(cosmos.database == nil and cosmos.network == nil) end
    "#,
        mysql = cfg!(feature = "mysql"),
        redis = cfg!(feature = "redis")
    );
    runtime.block_on(machine.exec(&source)).unwrap();
}
