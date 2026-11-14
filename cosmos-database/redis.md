# cosmos.database.redis

基于 lua-resty-redis 0.34 适配的异步 RESP2 客户端，复用 `cosmos.network.socket`，支持 TCP、Unix socket、TLS、普通命令、pipeline、事务和发布订阅。由 `cosmos-database` 的 `redis` feature 控制，默认启用；安装方式见 [模块说明](README.md)。

## 接口

`require("cosmos.database.redis")` 与 `cosmos.database.redis` 是同一张表。

| 接口 | 用途 |
| --- | --- |
| `redis.new()` | 创建独立连接 |
| `conn:connect(options_or_url)` | 建连，可使用 `redis://` / `rediss://` URL；也接受 `connect(host, port, options?)`、`connect("unix:/path", options?)` |
| `redis.client(options_or_url)` | 创建拥有独立空闲连接池的 client |
| `client:connect()` / `client:release(conn)` | 租借、归还连接；也可使用 `conn:set_keepalive()` 归还 |
| `conn:get_reused_times()` | 复用次数，首次连接为 0 |
| `conn:get(key)`、`conn:set(key, value, ...)` 等 | 按方法名生成 Redis 命令，支持通用命令与 Redis 模块命令 |
| `conn:execute(command, ...)` | 显式执行指定命令；`conn:command(...)` 保留为原生 Redis COMMAND，`conn:client(...)` 保留为原生 CLIENT |
| `conn:set_timeout(ms)` | 同时设置连接、发送、读取超时；0 表示无限等待 |
| `conn:set_timeouts(connect_ms, send_ms, read_ms)` | 分别设置三种超时 |
| `conn:init_pipeline(n?)` / `cancel_pipeline()` / `commit_pipeline()` | 缓存请求、取消未发送请求、一次发送并按顺序收取回复 |
| `conn:multi()` / `exec()` / `discard()` | Redis 事务；也支持 WATCH / UNWATCH 与 pipeline 中的事务 |
| `conn:subscribe(...)` / `psubscribe(...)` / `read_reply()` | 订阅频道、订阅模式、读取消息 |
| `conn:unsubscribe(...)` / `punsubscribe(...)` | 取消指定订阅，省略参数时取消对应种类的全部订阅 |
| `conn:hmset(key, table)` / `hmget(key, fields)` / `array_to_hash(array)` | 保留上游的哈希表便利接口 |
| `redis.add_commands(...)` / `redis.register_module_prefix(prefix)` | 注册命令或模块前缀，例如先 `conn:json()` 再调用命令 |
| `conn:close()` / `client:close()` | 立即关闭连接、关闭 client 的空闲池并拒绝后续租借 |
| `redis.parse_url(url)` | 解析 URL，支持百分号编码、IPv6、用户名/密码和数据库编号 |

方法参数使用字符串或有限数值；字符串保留 NUL、非 UTF-8 和 CRLF 字节。大整数作为参数时应传字符串。回复中的有符号 64 位整数在 Lua 可精确表达的范围内返回 number，超出 `±(2^53-1)` 返回十进制字符串。

空 bulk string 返回 `""`，Redis nil（包括空缺的 bulk / array）返回 `cosmos.null`，也可使用 `redis.null`。顶层 Redis 错误返回 `false, err`；嵌套数组或 pipeline 中的错误保留为 `{false, err}`。连接、超时或协议错误返回 `nil, err`，并关闭受影响的流。

## 连接与边界

选项包括 `host` / `port`（默认 `127.0.0.1:6379`）或 Unix socket 的 `path`，以及 `username`、`password`、`db`（默认 0）。认证和 SELECT 在首次建立连接时完成；`rediss://` 或 `ssl=true` 启用 TLS，默认验证证书，可指定 `ctx`、`server_name` 或显式 `ssl_verify=false`。URL 查询参数只接受 `ssl_verify`。

`timeout` 默认 5000 毫秒；`connect_timeout`、`send_timeout`、`read_timeout` 可分别覆盖。超时作用于每次收发，包括阻塞命令和发布订阅读取；失败或取消后连接不会继续复用。

请求与回复默认上限各 64 MiB，可用 `max_request_size` / `max_reply_size` 调整；一个 pipeline 的请求、回复分别共用该字节上限。数组默认最多 65536 个元素（`max_array_len`），嵌套默认最多 128 层（`max_depth`），pipeline 最多 10000 条命令。回复全部读入内存。

当前使用 RESP2；HELLO 3、MONITOR、复制流、分片订阅及改变回复模式的 CLIENT 子命令会被明确拒绝。Pub/Sub 命令不能放入 pipeline 或 MULTI；普通命令等待响应时可缓冲已订阅频道的消息，使用 `read_reply()` 取出。

## 连接池

每个 `redis.client` 独立持有空闲池，`pool_size` 默认 10 限制空闲数量，另有下文的 max_connections/max_waiters/acquire_timeout 预算；`idle_timeout` 默认 60000 毫秒，0 表示不过期。不提供命名全局池、backlog 或 tcp_keepalive 选项。

租借前用 PING 检查连接，失效则重新建立。归还后旧句柄失效。未收完 pipeline、仍在订阅 / MULTI / WATCH 状态、收发失败或取消的连接不能回池；公开调用 AUTH、SELECT 等改变会话的命令后，也会保守地禁止归还。使用完应关闭该连接，再从 client 租借新的连接。

## 验证

完整示例见 [examples/redis.lua](../examples/redis.lua)：通过 `COSMOS_REDIS_URL` 或脚本参数传入地址，每轮使用独立键前缀与 TTL，结束清理。该回归还会创建并删除自己的临时 ACL 用户，需要相应管理权限；不会修改已有用户。

```sh
COSMOS_REDIS_URL='redis://127.0.0.1:6379/0' ./target/debug/cosmos-cli examples/redis.lua
```

离线测试覆盖异常 RESP、二进制与嵌套错误、整数精度、资源上限、未读完 pipeline / PubSub 的超时关闭，以及校验证书的 TLS 和 Unix socket。

## 资源作用域

两种数据库共享有界租借预算：max_connections 默认 pool_size（10），max_waiters 默认 min(4*max_connections,65536)，acquire_timeout 默认 5000 毫秒、0 只尝试立即获取。max_connections 限制租借/建连，pool_size 继续限制空闲数量；获取前先占有界等待名额，超过队列直接报错。stats() 返回 in_use/waiting/idle/max_connections/max_waiters/closed，空闲过期使用单调时钟。

`client:with_connection(callback, ...)` 成功归还并保留回调返回值；抛错或返回 `nil/false, err` 时关闭连接并返回错误。`client:transaction(callback, ...)` 在同一租借上 MULTI，回调成功后 EXEC，并返回 EXEC 的结果数组；失败时 DISCARD/关闭。EXEC 内逐条 Redis 错误按原协议返回，不承诺 SQL 式回滚；WATCH/乐观重试由应用自行组合，不自动重试事务。

client.close 会关闭空闲池并唤醒等待者，已借出的连接结束后关闭，不重新入池。取消中的 socket I/O 立即释放，Lua 不执行被取消作用域的 finally；不可达租借配额在 GC 时归还。kill 是请求，join 后才完成执行清理；调用方手动保留租借时应主动 close。具体预算、取消、排队超时和事务验证见 [database_resources.lua](../examples/database_resources.lua)。
