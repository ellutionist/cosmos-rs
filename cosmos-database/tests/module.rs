//! 插件隔离、URL 转换与显式启用的真实数据库回归。
#![cfg(feature = "mysql")]

use cosmos::Machine;
use cosmos_database::Database;

#[test]
fn database_registration_and_url_options_are_isolated() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Database).unwrap();
    runtime.block_on(machine.exec(r#"
        local mysql = require('cosmos.database.mysql')
        assert(require('cosmos.database').mysql == mysql)
        assert(mysql == cosmos.database.mysql and mysql.null == cosmos.null)
        local options = assert(mysql.parse_url('mysql://u%40ser:p%3Ass@[::1]:3307/%E6%B5%8B%E8%AF%95?charset=utf8mb4&ssl=true&ssl_verify=false'))
        assert(options.host == '::1' and options.port == 3307)
        assert(options.user == 'u@ser' and options.password == 'p:ss' and options.database == '测试')
        assert(options.ssl and not options.ssl_verify and options.charset == 'utf8mb4')
        assert(mysql.parse_url('mysql://localhost').port == 3306)
        for _, url in ipairs({'http://localhost/db', 'mysql:///db', 'mysql://localhost/a/b',
                               'mysql://u:%GG@localhost/db', 'mysql://u:%00@localhost/db',
                               'mysql://localhost/db#fragment', 'mysql://localhost/db?ssl=maybe',
                               'mysql://localhost/db?unknown=1'}) do
            local value, err = mysql.parse_url(url)
            assert(value == nil and type(err) == 'string')
        end
        local _, error = mysql.parse_url('mysql://user:secret@localhost:bad/db')
        assert(not error:find('secret', 1, true))
        assert(mysql.quote_sql_str("a'\0\255") == "X'612700ff'")
        assert(mysql.client({pool_size=0}) == nil)
        assert(mysql.client({backlog=1}) == nil)
        assert(mysql.client({idle_timeout=0/0}) == nil)
        local client = assert(mysql.client('mysql://localhost/db'))
        client:close()
        assert(client:connect() == nil)
        assert(mysql.new():set_keepalive() == nil)
    "#)).unwrap();
    let mut isolated = Machine::new(runtime.handle().clone()).unwrap();
    runtime
        .block_on(isolated.exec(
            "assert(cosmos.database == nil); assert(not pcall(require, 'cosmos.database.mysql'))",
        ))
        .unwrap();
    // 重复注入不能破坏父模块身份或调用方增补的字段。
    runtime
        .block_on(machine.exec("database_before = cosmos.database; cosmos.database.marker = 42"))
        .unwrap();
    machine.add_module(Database).unwrap();
    runtime
        .block_on(
            machine.exec(
                "assert(cosmos.database == database_before and cosmos.database.marker == 42)",
            ),
        )
        .unwrap();
}

#[test]
#[ignore = "需要显式设置 COSMOS_MYSQL_URL，脚本只操作会话临时表"]
fn mysql_example_against_explicit_database() {
    let url = std::env::var("COSMOS_MYSQL_URL").expect("请设置 COSMOS_MYSQL_URL");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Database).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            machine.exec_with_args(
                "=mysql-example",
                include_str!("../../examples/mysql.lua"),
                &[url],
            ),
        )
        .await
        .unwrap()
        .unwrap();
    });
}
