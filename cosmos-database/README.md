# cosmos-database

cosmos 的异步数据库插件，目前提供 `cosmos.database.mysql`。MySQL 协议基于 lua-resty-mysql 0.29 适配，保留 Lua 实现；传输、TLS、编解码与密码学复用 cosmos 的 Rust 模块，不依赖 OpenResty 或 MySQL C 客户端库。原始版权与许可见 [第三方声明](THIRD_PARTY_NOTICES.md)。

CLI 默认启用 `database` feature。Rust 宿主使用 `machine.add_module(cosmos_database::Database)?`；插件自动安装所需的 Network、Encoding、Encryption，模块之间的 `require` 无需配置文件搜索路径。

## 使用

入口为 `require("cosmos.database.mysql")`，与 `cosmos.database.mysql`、`require("cosmos.database").mysql` 是同一张表。

| 接口 | 用途 |
| --- | --- |
| `mysql.new()` | 创建独立连接，随后调用 `connect(options_or_url)` |
| `mysql.client(options_or_url)` | 创建拥有独立空闲连接池的 client |
| `client:connect()` | 租借连接；没有可复用连接时建立并认证 |
| `conn:query(sql, estimated_rows?)` | 返回行数组或包含 `affected_rows` / `insert_id` 等字段的 OK 结果；失败返回 `nil, err, errno?, sqlstate?` |
| `conn:send_query(sql)` / `conn:read_result(estimated_rows?)` | 分开发送与读取；多结果集返回第二值 `"again"`，须继续 `read_result` |
| `conn:ping()` / `conn:server_ver()` | 检查连接、读取服务端版本 |
| `conn:set_timeout(ms)` / `conn:set_compact_arrays(bool)` | 设置每次收发超时、将结果行切换为按列顺序的数组 |
| `client:release(conn)` / `conn:set_keepalive()` | 将干净连接归还所属 client，旧连接句柄立即失效 |
| `conn:get_reused_times()` | 当前连接的复用次数，首次连接为 0 |
| `conn:close()` / `client:close()` | 关闭单连接、关闭 client 的空闲池并拒绝新的租借 |
| `mysql.parse_url(url)` | 将 `mysql://user:password@host:port/database` 转成选项表，支持百分号编码和 IPv6 |
| `mysql.quote_sql_str(bytes)` | 返回 `X'…'` 形式的 SQL 二进制字符串字面量，支持 NUL，不依赖反斜线转义模式；不用于引用标识符 |

完整示例及回归见 [examples/mysql.lua](../examples/mysql.lua)。使用测试数据库时通过环境变量传入凭据：

```sh
COSMOS_MYSQL_URL='mysql://user:password@127.0.0.1:3306/example' ./target/debug/cosmos-cli examples/mysql.lua
```

示例使用查询、会话临时表，并主动终止自己创建的测试连接以验证池的失效重连，不修改已有业务表。在已导出 `COSMOS_MYSQL_URL` 的 shell 中，真实数据库测试需要显式启用：

```sh
cargo test -p cosmos-database --test module -- --ignored
```

## 连接选项

选项表接受 `host` / `port`（默认 `127.0.0.1:3306`），或 Unix socket 的 `path`；认证使用 `user`、`password`、`database`。`charset` 默认 `utf8mb4`，`timeout` 默认 5000 毫秒，0 表示不限制；`max_packet_size` 默认 1 MiB，限制单个逻辑包，支持大于 16 MiB 的物理包分帧。

TLS 使用 `ssl=true`，默认验证证书；可注入 `ctx`（`cosmos.network.tcp.tls.context`）并指定 `ssl_server_name`。`ssl_verify=false` 显式关闭证书校验。TLS 在收到 MySQL greeting、发送 SSLRequest 后升级原连接，Unix socket 同样可用。URL 查询参数接受 `charset`、`ssl`、`ssl_verify`。

支持 `mysql_native_password`、`caching_sha2_password`、`sha256_password` 和 MariaDB `client_ed25519`；完整 SHA-2 认证在明文 TCP 上使用 RSA-OAEP-SHA1，可通过 `public_key` 固定 PEM 公钥。`mysql_clear_password` 仅接受 TLS 或 Unix socket。

SQL NULL 返回 `cosmos.null`（也可用 `mysql.null`），保留行字段与数组位置。BIGINT 保持字符串以保留精度，其余数字沿用上游转换规则，包括 DECIMAL 转 Lua number。当前提供文本查询协议，尚不提供服务端预编译语句。

## 连接池

池按 client 实例隔离，不创建命名全局池。`pool_size` 默认 10，只限制空闲连接数量，不限制活跃连接并发数；`idle_timeout` 默认 60000 毫秒，0 表示不过期。不支持 `pool` / `backlog`，传入时返回明确错误。

租借时使用 COM_PING 检查空闲连接，失效则重新连接。收发超时、取消或协议异常的连接不能回池；多结果集必须读完，事务须提交或回滚，且 autocommit 应开启。归还后会转移底层连接，旧句柄无法操作后续租借。普通 SQL 错误在完整读取 ERR 包后允许继续使用连接。

连接仍保留会话设置，调用方应恢复自己修改的会话变量和数据库选择。`client:close()` 立即关闭空闲连接；已经租借的连接由调用方关闭，或在归还时丢弃。

离线测试覆盖认证分支、长密码 RSA scramble、MariaDB 签名、异常序号、截断包、包大小限制及 16 MiB 分帧，不依赖数据库服务端。
