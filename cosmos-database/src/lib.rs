//! 数据库插件：组合 cosmos 网络、编解码和加密能力，协议实现随 crate 内嵌。
#![warn(unreachable_pub)]

#[cfg(feature = "mysql")]
mod mysql;
#[cfg(feature = "mysql")]
mod mysql_binary;
#[cfg(any(feature = "mysql", feature = "redis"))]
mod options;
#[cfg(feature = "redis")]
mod redis;

/// 根据 mysql / redis feature 注册 cosmos.database 的数据库客户端。
pub struct Database;

impl cosmos::Module for Database {
    fn register(&self, _machine: &mut cosmos::Machine) -> Result<(), String> {
        #[cfg(any(feature = "mysql", feature = "redis"))]
        let machine = _machine;
        #[cfg(any(feature = "mysql", feature = "redis"))]
        machine.load("cosmos.database.pool.init", include_str!("../lua/pool.lua"))?;
        #[cfg(any(feature = "mysql", feature = "redis"))]
        machine.add_module(cosmos_network::Network)?;
        #[cfg(feature = "mysql")]
        {
            machine.add_module(cosmos_encoding::Encoding)?;
            machine.add_module(cosmos_encryption::Encryption)?;
            machine.register::<mysql::Mysql>();
            machine.load("cosmos.database.mysql", include_str!("../lua/mysql.lua"))?;
        }
        #[cfg(feature = "redis")]
        {
            machine.register::<redis::Redis>();
            machine.load("cosmos.database.redis", include_str!("../lua/redis.lua"))?;
        }
        Ok(())
    }
}
