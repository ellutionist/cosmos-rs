//! 两种数据库的预算验证与显式服务器资源管理验收。
#![cfg(any(feature = "mysql", feature = "redis"))]
use cosmos::Machine;
use cosmos_database::Database;
#[test]
fn pool_options_failed_dials_and_repeated_registration_preserve_budgets() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Database).unwrap();
    runtime.block_on(machine.exec(&format!(r#"
        clients={{}}
        for _,name in ipairs({{'mysql','redis'}}) do
            local module=cosmos.database[name]
            if module then
                for _,opts in ipairs({{{{max_connections=0}},{{max_connections='bad'}},{{max_waiters=-1}},{{acquire_timeout=0/0}}}}) do
                    local value,err=module.client(opts);assert(value==nil and type(err)=='string')
                end
                local client=assert(module.client({{host='127.0.0.1',port={port},timeout=20,max_connections=1,max_waiters=0}}))
                assert(client:stats().in_use==0 and client:stats().max_connections==1)
                for i=1,3 do local value,err=client:connect();assert(value==nil and type(err)=='string');assert(client:stats().in_use==0 and client:stats().waiting==0) end
                clients[name]=client
            end
        end
    "#))).unwrap();
    machine.add_module(Database).unwrap();
    runtime.block_on(machine.exec("for _,client in pairs(clients) do client:close();assert(client:stats().closed and client:connect()==nil) end")).unwrap();
}
#[cfg(all(feature = "mysql", feature = "redis"))]
#[test]
#[ignore = "requires COSMOS_MYSQL_URL and COSMOS_REDIS_URL; creates and removes isolated test objects"]
fn prepared_and_resource_example_against_explicit_servers() {
    let mysql = std::env::var("COSMOS_MYSQL_URL").expect("set COSMOS_MYSQL_URL");
    let redis = std::env::var("COSMOS_REDIS_URL").expect("set COSMOS_REDIS_URL");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Database).unwrap();
    runtime
        .block_on(machine.exec_with_args(
            "=database_resources",
            include_str!("../../examples/database_resources.lua"),
            &[mysql, redis],
        ))
        .unwrap();
}
