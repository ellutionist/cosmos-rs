# cosmos-logging

独立的纯文本日志插件，提供级别过滤、结构化字段、文件与控制台输出、轮转和刷新。每条日志自动包含 Lua 源文件名和行号。CLI 通过 `logging` 或 `full` feature 启用，默认关闭；Rust 宿主使用 `machine.add_module(cosmos_logging::Logging)?`，不依赖其他插件。

入口为 `require('cosmos.log')`，完整用法见 [logging.lua](../examples/logging.lua)。记录形态如下：

```text
2026-11-14T04:00:00.000Z INFO app.lua:12 request completed duration_ms=8 request="a1"
```

只输出纯文本，不提供 JSON 输出；`format` / `console_format` 若传入，只接受 `text`，其他值返回 `nil, err`。消息、路径和字段中的换行及控制字符会转义，记录保持单行。时间戳使用 UTC 真实系统时间。

| 接口 | 说明 |
| --- | --- |
| `log.new(options?)` | 异步创建 logger，默认 info 级别、stderr 输出 |
| `logger:trace/debug/info/warn/error(message, fields?)` | 写入完成返回 true，被过滤返回 false；失败返回 nil, err |
| `logger:write(level, message, fields?)` | 显式选择事件级别，不接受 off |
| `logger:child(fields)` | 合并固定上下文，调用字段覆盖同名上下文字段 |
| `logger:set_level(level)` / `:enabled(level)` | 设置共享过滤级别 / 查询当前是否启用 |
| `logger:flush(sync?)` | 等待此前已接纳写入并刷新；sync=true 额外同步文件数据 |
| `logger:close()` | 停止接纳、等待此前写入、刷新并关闭文件；可重复调用 |

级别依次为 trace/debug/info/warn/error/off；父子 logger 共享输出、过滤与关闭状态，各自的固定上下文独立。字段根为字符串键表，值支持字符串、有限数字、布尔、cosmos.null、嵌套对象及连续数组；空表作为对象。对象渲染为 `(key=value)`，数组为 `[value, value]`，字符串带引号。字段读取不执行元方法，不能包含函数、其他 userdata、循环、混合键或稀疏数组。

| 创建选项 | 默认值与语义 |
| --- | --- |
| `file` | 可选文件路径，Unix 保留原生路径字节；自动创建父目录 |
| `console` | stderr；可选 stdout、none、host；none 必须配 file |
| `level` | info |
| `rotation` | daily（UTC）；none 使用固定文件名 |
| `max_bytes` | 10 MiB；写入前按完整记录轮转，0 关闭大小轮转 |
| `keep_files` | 每个文件保留 5 份大小归档，范围 0..64；0 不保留 |
| `keep_days` | 保留 7 个 UTC 日期，范围 0..3650；0 不清理，仅适用于 daily |

轮转选项必须配合 file。daily 把 `app.log` 写为 `app.YYYY-MM-DD.log`，大小归档追加 `.1`、`.2`。超过阈值的单条记录单独写入，不拆行；保留期在打开/切换日期与轮转时检查，仅匹配当前文件模板的精确名字。多个独立 logger 或进程不要共同轮转同一文件，需共享同一个 logger/child。外部改名或截断文件不自动重新打开。

源位置在进入 Rust 桥接时复制，先于异步挂起；异步线程不读取 Lua。通常得到真实路径和调用行，字符串代码块使用其调试标签；删除调试信息的字节码可能无法获得行号，此时为 0。日志包装函数显示实际调用日志 API 的位置。

`console='host'` 显式发送到调用方当前 tracing subscriber，target 为 `cosmos.lua`，包含 `message`、`lua_file`、`lua_line`、文本 `fields`；message 本身也带 Lua 文件与行号。插件不安装或修改全局 subscriber，宿主负责使用文本 subscriber；无 subscriber 时事件不会显示。默认 stderr/stdout 与文件直接写纯文本。

写入、文件打开、轮转和 flush 在 Tokio 原生工作线程执行；同一 logger 串行写入，Lua 仍在 Machine 的 LocalSet。每个 logger 最多接纳 128 个未完成写入，满后立即返回 `log queue is full`；消息最多 64 KiB，字段字符串总计 64 KiB、4096 节点、16 层，编码字段最多 128 KiB、单条记录最多 256 KiB。取消未开始的作业释放预算，已经开始的操作系统写入不能撤回，仍持有预算直到结束；Machine 销毁先释放 Lua 等待，不等待每个阻塞输出完成。

请在应用停止流程中显式等待 close/flush；丢弃句柄只关闭文件描述符，不承诺同步落盘。写入或轮转的 I/O 失败会返回错误并使后续写入继续报告该故障，不静默丢弃或降级输出。输入验证失败不破坏 logger。与请求 ID、日志分类、业务保留策略和服务停止顺序有关的规则由应用组织。
