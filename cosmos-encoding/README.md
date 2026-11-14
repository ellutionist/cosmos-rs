# cosmos-encoding

cosmos 的 Rust 原生编解码插件，支持包含 NUL、非 UTF-8 字节的 Lua 字符串。

CLI 默认启用 `encoding` feature。Rust 宿主通过 `machine.add_module(cosmos_encoding::Encoding)?` 安装。

| 模块 | 接口 |
| --- | --- |
| `cosmos.encoding.hex` | `encode(data)`、`decode(text)`，编码输出小写十六进制 |
| `cosmos.encoding.base64` | `encode(data)`、`decode(text)`，标准 Base64，保留 padding |
| `cosmos.encoding.base64` | `encode_urlsafe(data)`、`decode_urlsafe(text)`，URL-safe Base64，不带 padding |
| `cosmos.encoding.uri` | `encode(data)`、`decode(text)`，URI 组件百分号编解码 |
| `cosmos.encoding.json` | `encode(value)`、`decode(text[, options])`，基于 sonic-rs |

每个子模块的 `require` 入口与 `cosmos.encoding` 的对应字段是同一张表。Hex、Base64、URI 解码成功返回原始字节字符串，非法编码返回 `nil, err`。

URI 编码保留字母、数字和 `-._~`；解码严格校验 `%XX`，`+` 保持字面值。Base64 标准与 URL-safe 形式分别校验各自的字母表和 padding 规则。

完整用法及标准向量见 [encoding.lua](../examples/encoding.lua)。只启用该 CLI 插件可使用 `--no-default-features --features encoding` 构建。

## JSON

使用 `require('cosmos.encoding.json')`。`encode` 返回 JSON 字符串，`decode` 返回 Lua 值；数据非法时返回 `nil, err`。JSON 字符串与对象键必须是有效 UTF-8，二进制内容请先用 Base64 编码。

- JSON `null` 对应 `cosmos.null`，也可写作 `json.null`；编码 Lua `nil` 同样得到 `null`。解码后的 null 为真值，不会删除表字段或数组元素。
- 连续的 `1..n` 整数键表编码为数组，字符串键表编码为对象，普通空表默认编码为 `{}`。`json.array([table])` / `json.object([table])` 可显式标记形状；省略参数时创建空表。解码保留空数组和空对象的区别。
- 不支持稀疏数组、混合键、循环引用、函数、任意 userdata、NaN 或无穷数。数组空位使用 `cosmos.null`。对象重复键解码时最后一个值生效，编码不保证对象键顺序。
- 默认将安全整数和普通小数转换成 Lua number；超出安全整数范围的整数、浮点溢出或下溢的非零数保留为精确数字 userdata，`:text()` / `tostring()` 可读取原文，重新编码仍为 JSON 数字。普通小数和指数形式默认遵循 Lua 双精度舍入语义。
- `decode(text, {lossless_numbers=true})` 将所有数字保留为精确数字，适用于金额、大整数和原文往返；`json.number.new(text)` 可直接构造同种数字。已在 Lua number 中丢失的精度无法恢复。

单次编解码最多 64 层容器、100 万个值节点，输入与输出各限 64 MiB；编码时字符串、对象键和精确数字另共用 64 MiB 原始字节预算。接口同步运行在 Machine 所在线程，不会异步卸载 CPU 工作。容器标记使用 `__jsontype='array'/'object'` 元表字段；已有自定义元表的表需自行添加该字段，标记函数不会覆盖其元表。

sonic-rs 根据编译目标选择 SIMD 后端，缺少对应特性时使用自身通用实现，不切换到 serde_json。项目不强制启用 `target-cpu=native`；自行启用后需匹配部署 CPU。

完整用法与边界回归见 [json.lua](../examples/json.lua)。
