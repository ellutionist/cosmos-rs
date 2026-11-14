# cosmos-encryption

cosmos 的 Rust 原生加密插件，提供增量摘要、HMAC、Argon2 密码哈希、安全随机数、RSA-OAEP 和 Ed25519。基于 RustCrypto、ed25519-dalek 与操作系统随机源，不依赖 OpenSSL。字节参数支持 NUL 和非 UTF-8 数据，摘要和密文返回原始字节字符串。

CLI 默认启用 `encryption` feature；Rust 宿主通过 `machine.add_module(cosmos_encryption::Encryption)?` 安装。插件独立于 `encoding`，需要文本结果时再组合 Hex / Base64。原认证用法见 [encryption.lua](../examples/encryption.lua)，增量文件摘要、HMAC、密码与随机数见 [crypto.lua](../examples/crypto.lua)，后者需传入一个已存在的临时父目录。

## 摘要与消息认证

| 接口 | 用法 |
| --- | --- |
| `cosmos.encryption.sha1/sha256/sha512(data)` | 保留既有同步一次性摘要入口，返回原始字节，可在 require 初始化阶段调用 |
| `cosmos.encryption.digest.new(algorithm)` | 创建增量摘要；算法为 `sha1` / `sha256` / `sha512` |
| `cosmos.encryption.digest.sum(algorithm, data)` | 一次性摘要 |
| `cosmos.encryption.hmac.new(algorithm, key)` | 创建同一组算法的增量 HMAC |
| `cosmos.encryption.hmac.sum(algorithm, key, data)` | 一次性 HMAC |
| `ctx:update(data)` | 追加一个二进制块，成功返回 true |
| `ctx:finalize()` | 消费状态并返回完整原始摘要/认证码 |
| `ctx:verify(expected)` | 消费状态并验证完整原始摘要/认证码；匹配为 true，不匹配或长度不同为 false |
| `ctx:close()` / `ctx:is_closed()` | 幂等关闭 / 查询是否关闭或已完成 |

增量上下文是共享句柄，多个协程的操作串行执行，不复制摘要状态；需要明确更新顺序时由应用按顺序调用。完成或关闭后再操作返回 `nil, "closed"`。等待同一上下文的锁时取消不影响其他调用；已进入操作后取消或计算失败会关闭上下文，避免复用不确定的中间状态。显式 close 唤醒等待者。

新增摘要/HMAC 接口每次输入最多 64 MiB，增量总长度不限；HMAC key 最多 64 KiB。大文件可组合 `cosmos.fs.file:read()` 分块追加，无需全量读入。HMAC 验证使用 RustCrypto 的完整认证码校验，摘要验证使用 subtle；不要用 Lua 字符串相等代替消息认证校验。标准向量来自 [RFC 2202](https://www.rfc-editor.org/rfc/rfc2202.html) 和 [RFC 4231](https://www.rfc-editor.org/rfc/rfc4231.html)。

## 密码

`cosmos.encryption.password.hash(password, options?)` 每次生成新的 16 字节随机 salt，返回标准 Argon2id v19 PHC 字符串，可直接存入数据库。密码是原始字节，允许空串，最多 4096 字节；长度、复杂度、登录重试等应用策略由业务层决定。

| options | 默认值 | 支持范围 |
| --- | --- | --- |
| `memory_kib` | 19456 | 8–65536 KiB，且至少为 `8 * parallelism` |
| `iterations` | 2 | 1–10 |
| `parallelism` | 1 | 1–4 个算法 lane，不代表创建等量 Tokio 线程 |
| `output_length` | 32 | 16–64 字节 |

`password.verify(password, encoded)` 从 PHC 字符串读取算法和成本参数，正确返回 true，密码不匹配返回 false；格式错误、缺少字段、未知算法/参数或超出上述成本上限返回 `nil, err`。接受 Argon2id / Argon2i / Argon2d 的 v16 / v19，PHC 仅允许 `m,t,p` 成本参数、8 字节以上的 salt 和 16–64 字节输出，文本最多 512 字节；不支持需要外部 secret 的 PHC 扩展。成本检查在分配工作区之前完成。

默认参数与 Librarian 原后端的 RustCrypto `Argon2::default()` 对齐，既有 `$argon2id$v=19$m=19456,t=2,p=1$...` 数据可以直接验证。互操作测试同时验证原生库生成的哈希和 Cosmos 生成的哈希。计算使用 [RustCrypto Argon2](https://docs.rs/argon2/0.5.3/argon2/)；本模块不提供密钥派生或密码加密接口。

## 随机数

`cosmos.encryption.random.bytes(size)` 返回操作系统安全随机字节，支持 0–1 MiB；`random.integer(min, max)` 在包含两端的整数范围内取值，参数须是 `[-(2^53-1), 2^53-1]` 内的精确整数。整数使用 u64 拒绝采样，避免浮点缩放和取模偏差；`min == max` 返回该值。操作系统随机源失败返回 `nil, err`，不会退化成 `math.random` 或固定 seed。

## 异步与资源边界

新增摘要更新/完成、`digest.sum` / `hmac.sum`、密码与随机数操作由 Tokio 阻塞工作线程执行；Lua 仍可顺序调用，等待时仅挂起当前协程。HMAC 构造的 key 处理有 64 KiB 上限；既有根表 SHA 和 RSA/Ed25519 接口继续同步执行，兼容 require 的模块初始化；大块摘要应使用新增异步接口或文件分块。

所有 Machine/Runtime 共享最多 2 个原生计算作业、32 个已接纳作业（包括运行中）的预算。等待计算额度时保留 Lua 参数锚点，拿到额度后才在宿主线程准备拥有型 Rust 数据；工作线程不持有 Lua 引用。队列满返回 `nil, "crypto worker queue is full"`。此预算约束原生工作，不限制应用自身的 Lua 内存或协程数量。

取消或销毁 Machine 可以撤销尚未开始的作业；已运行的原生计算不能强制中断，会继续占用额度直到结束，结果随后丢弃。原生密码副本、派生输出和 Argon2 工作区在回收时显式清零；Lua 字符串不可变，不承诺清除 VM 中的密码副本，也不承诺清除第三方摘要/HMAC 内部表示。宿主仍负责 Runtime 生命周期。

## RSA 与 Ed25519

| 模块 | 接口 |
| --- | --- |
| `cosmos.encryption.rsa` | `new(public_key_pem)`，读取 SPKI `PUBLIC KEY` 或 PKCS#1 `RSA PUBLIC KEY` |
| RSA 公钥对象 | `size()` 返回字节数；`encrypt(data, digest?)` 使用 RSA-OAEP，摘要可选 `sha1` / `sha256`，默认 `sha1` 对齐 MySQL 认证 |
| `cosmos.encryption.ed25519` | `from_seed(seed)`，使用 32 字节 seed 创建标准 Ed25519 密钥 |
| `cosmos.encryption.ed25519` | `from_key_material(material)`，直接按 SHA-512 扩展私密材料，兼容 MariaDB `client_ed25519` |
| Ed25519 密钥对象 | `public_key()` 返回 32 字节公钥；`sign(message)` 返回 64 字节签名 |
| `cosmos.encryption.ed25519` | `verify(public_key, message, signature)`，通过返回 true，签名不匹配返回 false |

RSA-OAEP 的 MGF1 与 OAEP 使用相同摘要，每次加密使用操作系统安全随机数。Ed25519 公钥从密钥对象内部导出，私钥表示和扩展状态不向 Lua 暴露。参数长度、PEM 或算法错误返回 `nil, err`。

可通过 `--no-default-features --features encryption` 单独构建；[crypto.lua](../examples/crypto.lua) 不依赖其他插件，原 [encryption.lua](../examples/encryption.lua) 为展示文本结果同时使用 Encoding。
