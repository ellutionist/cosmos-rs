# cosmos-encryption

cosmos 的 Rust 原生摘要与非对称算法插件，基于 RustCrypto 和 ed25519-dalek，不依赖 OpenSSL。接口同步执行，字节参数支持 NUL 和非 UTF-8 数据。

CLI 默认启用 `encryption` feature。Rust 宿主通过 `machine.add_module(cosmos_encryption::Encryption)?` 安装；模块独立于 `encoding`，摘要和密文均返回原始字节字符串。

| 模块 | 接口 |
| --- | --- |
| `cosmos.encryption` | `sha1(data)`、`sha256(data)`、`sha512(data)` |
| `cosmos.encryption.rsa` | `new(public_key_pem)`，读取 SPKI `PUBLIC KEY` 或 PKCS#1 `RSA PUBLIC KEY` |
| RSA 公钥对象 | `size()` 返回字节数；`encrypt(data, digest?)` 使用 RSA-OAEP，摘要可选 `sha1` / `sha256`，默认 `sha1` 对齐 MySQL 认证 |
| `cosmos.encryption.ed25519` | `from_seed(seed)`，使用 32 字节 seed 创建标准 Ed25519 密钥 |
| `cosmos.encryption.ed25519` | `from_key_material(material)`，将私密材料直接按 SHA-512 扩展，兼容 MariaDB `client_ed25519` 的密码派生 |
| Ed25519 密钥对象 | `public_key()` 返回 32 字节公钥；`sign(message)` 返回 64 字节签名 |
| `cosmos.encryption.ed25519` | `verify(public_key, message, signature)`，通过返回 `true`，签名不匹配返回 `false` |

RSA-OAEP 的 MGF1 与 OAEP 使用相同摘要，每次加密使用操作系统安全随机数。Ed25519 公钥由密钥对象内部导出，私钥表示和签名所需的扩展状态不向 Lua 暴露。

参数长度、PEM 或算法名称错误返回 `nil, err`。需要文本时，可组合 `cosmos.encoding.hex` 或 `cosmos.encoding.base64`。完整用法及 RFC 8032、MariaDB 回归向量见 [encryption.lua](../examples/encryption.lua)。

只启用该 CLI 插件可使用 `--no-default-features --features encryption` 构建；示例为了展示摘要和签名的文本结果，还会使用 `encoding`。
