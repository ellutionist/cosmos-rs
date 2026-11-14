# cosmos-encoding

cosmos 的 Rust 原生编解码插件，提供 Hex、Base64、URI 二进制编解码与 JSON、TOML、YAML 文本转换，以及 XML 文档与节点操作。

CLI 默认启用 `encoding` feature。Rust 宿主通过 `machine.add_module(cosmos_encoding::Encoding)?` 安装；插件需要 Rust 1.89+。

| 模块 | 接口 |
| --- | --- |
| `cosmos.encoding.hex` | `encode(data)`、`decode(text)`，编码输出小写十六进制 |
| `cosmos.encoding.base64` | `encode(data)`、`decode(text)`，标准 Base64，保留 padding |
| `cosmos.encoding.base64` | `encode_urlsafe(data)`、`decode_urlsafe(text)`，URL-safe Base64，不带 padding |
| `cosmos.encoding.uri` | `encode(data)`、`decode(text)`，URI 组件百分号编解码 |
| `cosmos.encoding.json` | `encode(value)`、`decode(text[, options])`，基于 sonic-rs |
| `cosmos.encoding.toml` | `encode(table)`、`decode(text)`，基于 Rust toml |
| `cosmos.encoding.yaml` | `encode(value)`、`decode(text)`，基于 serde-saphyr |
| `cosmos.encoding.xml` | `parse(text)`、节点构造与文档编辑，见 [XML 文档](xml.md) |

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

## TOML

使用 `require("cosmos.encoding.toml")`。`decode` 将配置文档读为 Lua 表，`encode` 将字符串键表写为 TOML；失败返回 `nil, err`。支持嵌套表、数组与表数组，根值必须是表，`cosmos.null` 会返回错误，不会静默丢弃字段。

日期时间返回 userdata，`:text()` / `tostring()` 读取文本，再次编码仍为 TOML 日期时间；`toml.datetime.new(text)` 可直接构造。普通 Lua 字符串始终编码为字符串，不会因内容像日期就改变类型。

TOML 整数限有符号 64 位，安全整数返回 Lua number，大整数复用精确数字 userdata；`toml.number.new(text)` 可构造精确整数。普通小数使用 Lua 双精度，支持 TOML 的 `inf` / `nan`。空容器可用 `toml.array()` / `toml.object()` 标记，解码结果保留形状。

基本用法见[项目 README](../README.md#tomlcosmosencodingtoml)，完整配置与边界示例见 [toml.lua](../examples/toml.lua)。

## YAML

使用 `require("cosmos.encoding.yaml")`。支持字符串、布尔值、整数、小数、数组、字符串键映射和 `cosmos.null`，`yaml.null` 是同一个空值；失败返回 `nil, err`。`yaml.array()` / `yaml.object()` 标记空容器，解码同样保留形状；YAML 的数字键、复杂键、重复键和自定义标签会返回错误。

支持锚点、别名与 `<<` 合并键，别名解码为独立 Lua 值。每次只解码一份文档，循环别名会报错；不启用文件包含或环境变量替换。布尔值按 YAML 1.2 处理，`yes` / `off` 等保持字符串。普通小数使用 Lua 双精度，NaN 与无穷数不接受。整数支持 i64/u64，超出 Lua 安全范围的值使用精确数字 userdata，超出 i64/u64 的整数字面量会报错，应加引号作为字符串保存。

`toml.number`、`yaml.number` 与 `json.number` 共用同一张类型表；TOML/YAML 仅接受其支持范围内的精确整数，小数 userdata 会报错，应使用 Lua number。YAML 中的普通日期文本作为字符串，编码 TOML 日期时间 userdata 时也输出字符串。

三种格式的文本、字符串值和映射键都要求有效 UTF-8；编解码最多 64 层容器、100 万个值节点、64 MiB 输入/输出，转换字符串与键共用字节预算。YAML 另限制 10000 个锚点/别名及 100 万次别名事件重放，展开后仍受节点和深度限制。编解码同步执行，输出不保留原文注释与排版。

用法见[项目 README](../README.md#yamlcosmosencodingyaml)，完整示例见 [yaml.lua](../examples/yaml.lua)。

## XML

XML 保留文档原文、节点顺序、属性、命名空间、混合文本、CDATA、注释和处理指令。支持节点构造及不可变的精确替换/插入，修改指定内容时保留其余原文；不经 JSON 转换。

解析和编辑在 Rust 工作线程执行，限制大小、深度、节点数与并发，不加载 DTD 或外部实体。接口及资源边界见 [XML 文档](xml.md)，完整用法见 [xml.lua](../examples/xml.lua)。
