# cosmos-database

cosmos 的异步数据库插件，提供 [MySQL](mysql.md) 和 [Redis](redis.md)。协议使用内嵌 Lua，传输、TLS 和基础转换复用 cosmos 的 Rust 能力，不依赖 OpenResty 或数据库 C 客户端库。上游来源与原始许可见 [第三方声明](THIRD_PARTY_NOTICES.md)。

Rust 宿主通过 `machine.add_module(cosmos_database::Database)?` 注入，插件自动加载所启用数据库的依赖。Lua 入口为 `cosmos.database.mysql`、`cosmos.database.redis`；对应的 `require` 返回同一张模块表。两种数据库都提供独立连接和按 client 实例隔离的空闲连接池。

## Feature

`cosmos-database` 默认启用全部数据库：

| Feature | 模块 | 自动安装的插件 |
| --- | --- | --- |
| `mysql` | `cosmos.database.mysql` | Network、Encoding、Encryption |
| `redis` | `cosmos.database.redis` | Network |

未启用的数据库不编译、不注册，也不安装其专用依赖。关闭全部数据库时，`Database` 注册为空操作。

嵌入应用只需要 Redis 时，可使用：

```toml
cosmos-database = { path = "../cosmos-rs/cosmos-database", default-features = false, features = ["redis"] }
```

CLI 默认不启用数据库。`database` feature 启用两种数据库，`full` 启用全部插件；也提供 `mysql`、`redis` 独立开关：

```sh
cargo build -p cosmos-cli --no-default-features --features redis
cargo build -p cosmos-cli --no-default-features --features mysql
```

CLI 的其他插件仍可按需通过 feature 组合。运行方式与 `Machine` API 见 [项目 README](../README.md)。

## 示例与验证

- [mysql.lua](../examples/mysql.lua)：认证、查询、多结果集、事务与连接池，连接配置使用 `COSMOS_MYSQL_URL`。
- [redis.lua](../examples/redis.lua)：二进制、pipeline、事务、发布订阅、ACL 与连接池，连接配置使用 `COSMOS_REDIS_URL`。

默认测试使用本地协议服务端，不连接外部数据库。在设置对应环境变量后，显式运行真实服务端回归：

```sh
cargo test -p cosmos-database --test module -- --ignored
cargo test -p cosmos-database --test redis -- --ignored
```

验证 feature 时单独选择 package，避免工作区其他依赖合并启用数据库 feature：

```sh
cargo test -p cosmos-database --no-default-features
cargo test -p cosmos-database --no-default-features --features mysql
cargo test -p cosmos-database --no-default-features --features redis
```
