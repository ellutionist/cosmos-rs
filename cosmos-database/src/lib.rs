//! 数据库插件：组合 cosmos 网络、编解码和加密能力，协议实现随 crate 内嵌。
#![warn(unreachable_pub)]

mod mysql;

/// 注册 cosmos.database，目前提供 MySQL 文本协议客户端。
pub struct Database;

impl cosmos::Module for Database {
    fn register(&self, machine: &mut cosmos::Machine) -> Result<(), String> {
        machine.add_module(cosmos_network::Network)?;
        machine.add_module(cosmos_encoding::Encoding)?;
        machine.add_module(cosmos_encryption::Encryption)?;
        machine.register::<mysql::Mysql>();
        machine.load("cosmos.database.mysql", include_str!("../lua/mysql.lua"))
    }
}
