# cosmos-encoding

cosmos 的 Rust 原生编解码插件，支持包含 NUL、非 UTF-8 字节的 Lua 字符串。

CLI 默认启用 `encoding` feature。Rust 宿主通过 `machine.add_module(cosmos_encoding::Encoding)?` 安装。

| 模块 | 接口 |
| --- | --- |
| `cosmos.encoding.hex` | `encode(data)`、`decode(text)`，编码输出小写十六进制 |
| `cosmos.encoding.base64` | `encode(data)`、`decode(text)`，标准 Base64，保留 padding |
| `cosmos.encoding.base64` | `encode_urlsafe(data)`、`decode_urlsafe(text)`，URL-safe Base64，不带 padding |
| `cosmos.encoding.uri` | `encode(data)`、`decode(text)`，URI 组件百分号编解码 |

每个子模块的 `require` 入口与 `cosmos.encoding` 的对应字段是同一张表。解码成功返回原始字节字符串，非法编码返回 `nil, err`。

URI 编码保留字母、数字和 `-._~`；解码严格校验 `%XX`，`+` 保持字面值。Base64 标准与 URL-safe 形式分别校验各自的字母表和 padding 规则。

完整用法及标准向量见 [encoding.lua](../examples/encoding.lua)。只启用该 CLI 插件可使用 `--no-default-features --features encoding` 构建。
