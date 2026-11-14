# builtin 文件系统

`cosmos.fs` 与 `cosmos.fs.path` 随每个 Machine 自动安装，CLI 关闭全部插件后也可使用。接口通过工作线程执行文件系统 I/O，等待期间其他 Lua 协程继续推进。

文件内容和路径均使用 Lua 字节串。Unix 路径保留原始字节，能否创建非 UTF-8 名称取决于文件系统；其他平台使用 UTF-8，不能表达的名称返回错误，不静默替换字节。路径不能包含 NUL。

实际用法见 [fs.lua](../examples/fs.lua)，传入已存在的临时父目录：

```sh
cosmos-cli examples/fs.lua /tmp
```

示例只创建并清理自己的排他子目录，覆盖二进制、并发、文件锁、原子写入和路径操作。

## 文件与目录

| 接口 | 行为 |
| --- | --- |
| `fs.open(path, mode?)` | 返回共享文件句柄，缺省只读；模式支持 `r`、`r+`、`w`、`w+`、`a`、`a+`、`wx`、`w+x`，`x` 要求目标不存在 |
| `fs.open(path, options)` | 选项为 `read/write/append/truncate/create/create_new`；没有写入标志时 read 缺省为 true |
| `file:read(size?)` | 缺省 8192 字节，允许短读；EOF 返回 `nil`，`read(0)` 返回空字符串；单次上限 16 MiB |
| `file:write(bytes)` | 写完全部输入，返回字节数；单次上限 64 MiB，错误前可能已写入部分内容 |
| `file:seek(offset, whence?)` | `set/cur/end`，缺省 set，返回新位置；offset 必须是整数 |
| `file:truncate(size)` | 设置文件长度，不改变当前游标 |
| `file:metadata()` | 查询打开的文件 |
| `file:flush()` / `file:sync(data_only?)` | flush 不保证持久化；sync 缺省同步数据和元数据，true 只同步数据 |
| `file:settimeout(ms)` | 设置后续操作的排队和 I/O 等待时限；0 清除，范围 0–1e12 ms |
| `file:lock(mode?, timeout_ms?)` | 建议锁，`exclusive/shared`，缺省 exclusive；timeout 缺省沿用文件超时，均未设置时无限重试；0 只尝试一次 |
| `file:unlock()` | 释放该句柄取得的锁，幂等；同一已加锁句柄不能再次 lock |
| `file:close()` / `file:is_closed()` | 幂等关闭并等待派发中的 I/O 结束；is_closed 查询逻辑关闭状态 |
| `fs.read_dir(path)` | 返回目录迭代句柄 |
| `directory:next()` | 返回 `{name, path, kind}`；结束返回 `nil`，顺序由文件系统决定 |
| `directory:close()` / `:is_closed()` / `:settimeout(ms)` | 生命周期与文件句柄一致 |

共享文件的游标和操作按句柄串行执行；不同文件可并行 I/O。锁是操作系统建议锁，行为取决于平台和文件系统；`lock` 的时限约束等待重试，单次系统调用仍受操作系统行为约束。

成功的修改操作返回 true，读取返回数据；普通 I/O 错误返回 `nil, err`，关闭为 `"closed"`，超时为 `"timeout"`，参数类型错误抛 Lua 错误。EOF 只有一个 nil，与 `nil, err` 区分。文件位置和长度限制在 Lua 能精确表达的整数范围内，超限明确报错。

## 文件系统操作

| 接口 | 行为 |
| --- | --- |
| `fs.read_file(path, limit?)` | 全量读取，缺省及最大上限 64 MiB；更大文件使用分块 read |
| `fs.write_file(path, bytes)` | 创建或截断写入，上限 64 MiB |
| `fs.write_atomic(path, bytes, options?)` | 在目标目录写临时文件后替换；`no_replace=true` 禁止覆盖，`sync=true` 同步文件并在 Unix 同步父目录 |
| `fs.try_exists(path)` | 跟随链接查询是否存在；不存在返回 false，权限及其他 I/O 错误不吞掉 |
| `fs.chmod(path, mode)` | Unix 权限位 0–07777；其他平台返回 unsupported |
| `fs.metadata(path)` / `fs.symlink_metadata(path)` | 前者跟随符号链接，后者查询链接本身 |
| `fs.canonicalize(path)` / `fs.read_link(path)` | 返回规范化的绝对路径 / 链接保存的目标路径 |
| `fs.create_dir(path)` / `fs.create_dir_all(path)` | 创建目录 / 递归创建父目录 |
| `fs.remove_file(path)` / `fs.remove_dir(path)` / `fs.remove_dir_all(path)` | 删除文件、空目录、目录树；调用方决定访问范围 |
| `fs.rename(from, to)` / `fs.rename_new(from, to)` | 标准重命名 / 原子禁止覆盖；rename_new 在 Linux、Android、Apple、Redox 使用原生能力，其他平台返回 unsupported |
| `fs.copy(from, to)` | 标准文件复制，可覆盖目标，返回字节数；不实现业务上的跨文件系统移动或回滚 |

metadata 返回 `kind`（file / directory / symlink / other）、`size`、`readonly`、可用的 `modified_ms/accessed_ms/created_ms` 和 Unix 权限位 `mode`（0–07777）。另外提供 `modified_ns/accessed_ns/created_ns` 十进制整数字符串，保留系统报告的纳秒精度，用于文件变化检测；不能先转成 Lua number。毫秒字段用于显示和普通日期处理，不支持的字段缺省。

原子写入替换普通文件时保留其权限，新文件使用临时文件的私有权限；不承诺保留所有者、ACL 或扩展属性。替换和持久化不是同一件事：sync 失败可能发生在替换之后。底层不支持原子操作时返回错误，不以先检查再覆盖来模拟原子性。

## 路径与取消语义

`fs.path` 提供 `join`、`parent`、`filename`、`stem`、`extension`、`is_absolute`、`normalize`、`starts_with`、`strip_prefix`、`is_normal_relative`。返回平台原生路径；join 的第二段为绝对路径时替换第一段。normalize 只处理词法上的点段，不解析链接，也不承担根目录访问策略。starts_with / strip_prefix 按路径组件匹配，不能把 root-other 当作 root 的子目录；is_normal_relative 对应非空且全部为普通组件的相对路径检查，应用在 canonicalize 后自行组合根目录访问策略。

I/O 错误有稳定英文前缀：`not found:`、`already exists:`、`permission denied:`、`unsupported:`、`would block:`、`cross-device:`、`not a directory:`、`directory not empty:`，其余为 `io:`。跨文件系统移动应只在 cross-device 时由应用组合复制、关闭和删除，不把任意重命名错误视为复制理由。sync 不支持时返回 unsupported，应用可自行选择暂存读回校验策略；原子接口不静默降低持久化保证。

- 排队等候句柄的操作被取消或超时，不改变句柄状态。
- I/O 已派发到工作线程后，取消或超时会关闭共享句柄，防止后续操作复用结果未知的游标；其他等待者得到 closed。
- 已运行的系统调用不能被 Rust Future 撤销，可能继续产生文件副作用；资源在调用完成后释放。close 本身也可能等待慢磁盘或网络文件系统。
- 独立的复制、写入、重命名等操作取消后仍可能完成。需要事务性归档的应用应自己组织暂存和回滚。
- 最后一个句柄或 VM 释放时回收资源；只有拥有的 Rust 数据进入工作线程，不发送 Lua 状态、引用或回调。
