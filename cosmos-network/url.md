# URL

`cosmos.network.http.url` 随 Network 安装，CLI 默认可用；不需要 encoding 或 text。Rust 宿主使用已有的 `machine.add_module(cosmos_network::Network)?`。模块字段与 `require('cosmos.network.http.url')` 共用同一张表。

基于 Rust `url` 的 WHATWG 解析器，支持通用绝对 URL、相对引用合并、组件读写和有序查询参数；HTTP 客户端也使用相同的解析/合并规则。完整用法与真实重定向验收见 [url.lua](../examples/url.lua)，HTTP 收发见 [http.md](http.md)。

## 解析与访问

`url.parse(text)` 返回不可变 URL 句柄。解析失败返回 `nil, err`；输入必须是有效 UTF-8。相对地址需要先取得基地址，再调用 `base:join(reference)`。`:string()` 返回标准化后的完整字符串，可传给 `client:request`。

| 接口 | 说明 |
| --- | --- |
| `:scheme()` / `:host()` | 协议、主机；无主机时 host 为 nil，IPv6 带方括号，Unicode 域名转 IDNA ASCII |
| `:username()` / `:password()` | userinfo 组件；用户名缺省为空串，密码缺省为 nil，保留组件中的百分号编码 |
| `:port()` / `:port_or_default()` | 显式非默认端口 / 显式或已知默认端口；没有对应值时 nil |
| `:path()` / `:path_segments()` | 原始编码路径 / 编码后的有序路径段；不可分段的 opaque URL 返回 `nil, err` |
| `:query()` / `:fragment()` | 保留百分号编码的原始组件；不存在时 nil，存在但为空时空串 |
| `:query_pairs()` | 解码后的 `{name, value}` 数组，保留顺序、重复键与空值 |
| `:can_be_base()` | 是否支持层级路径；opaque URL 的相对引用合并按 Rust url 规则处理 |
| `:origin()` | 标准化后的 tuple origin；opaque origin 为 nil |
| `:same_origin(other)` | 按协议、标准化主机和有效端口比较；不将两个 opaque origin 视作同源 |
| `:join(reference)` | 返回合并后的新 URL，支持相对路径、`..`、仅 query、仅 fragment、协议相对或绝对地址 |

默认端口会从序列化值中省略。join 在完整原文组件上执行：`?q=2` 替换查询，`#part` 保留路径和查询，空引用保留路径/查询并移除 fragment。userinfo、路径、查询和 fragment 不参与同源判断。

## 修改与编码

所有修改方法返回新句柄，原句柄及别名不变；失败返回 `nil, err`。表输入只读取实际字段与连续整数键，不使用 `__index` 补齐缺失内容。

| 接口 | 说明 |
| --- | --- |
| `:with_scheme(scheme)` | 修改协议；遵守 WHATWG 的 special/非 special 切换限制 |
| `:with_host(host)` | 替换主机；nil 尝试移除主机，HTTP(S) URL 不允许移除 |
| `:with_port(port)` | 0..65535 整数；nil 移除显式端口 |
| `:with_username(name)` / `:with_password(password)` | 修改 userinfo；空用户名或 nil 密码清除对应组件 |
| `:with_path(path)` | 修改整条路径，保留路径分隔及已有百分号编码；处理点段并编码需转义字符 |
| `:with_path_segments(segments)` | 使用原始 UTF-8 字符串数组构造路径；段内 `/`、`%`、`?`、`#` 会编码，`.` / `..` 段按 Rust url 规则跳过 |
| `:with_query(query)` / `:with_fragment(fragment)` | 设置原始组件；nil 移除，空串保留空组件 |
| `:with_query_pairs(pairs)` | 清空并重新编码整组查询参数；空数组保留空查询 `?` |
| `:append_query_pair(name, value)` | 保留现有查询的原始字节，只在末尾追加一个编码后的参数，允许重复键 |

query_pairs 使用 `application/x-www-form-urlencoded` 规则：空格编码为 `+`，字面量加号为 `%2B`，查询内的 `+` 解码为空格。with_query 不执行此解码/重编码；例如原始逗号、已有 `%XX` 和空格的表达方式可由调用方保留。with_query_pairs 会重新编码全部参数，裸键变为 `name=`；需要保持既有签名查询字节时用 append_query_pair。

path_segments 返回已经编码的片段，而 with_path_segments 接收未编码数据，直接把前者传给后者会再次编码 `%`。URI 单组件的百分号编解码仍属于可选的 `cosmos.encoding.uri`，它不会解析整个 URL，也不使用查询表单的加号规则；不要先编码整个 URL 再 parse。

WHATWG 解析不等于严格文本校验：它可能规范化主机、默认端口、点段及部分空白/控制字符，也接受部分不规范的百分号文本。查询解码沿用 Rust url 的 UTF-8 替换语义：无效编码字节变为 U+FFFD，无效 `%ZZ` 保持字面值。需要原始精确数据时读取 query/path，不先 query_pairs 再重建。域名白名单、协议选择和业务字段验证由应用决定。

## HTTP 集成与限制

HTTP 客户端仍接收 URL 字符串，只允许带主机的 http/https。请求和重定向会规范化 URL 并剥离 fragment；URL 中不支持隐式认证的 userinfo，存在用户名或密码时返回错误，请显式设置认证头。跨源重定向剥离 Authorization、Cookie、Proxy-Authorization；同源比较使用标准化协议/主机/有效端口。重定向次数、超时和一次性 body 的重放限制沿用现有 HTTP 语义。

每个 URL 文本/组件输入最多 64 KiB，标准化后的完整 URL 同样最多 64 KiB，HTTP 请求和重定向遵守相同限制。有序输入数组最多 1024 项，字符串总字节最多 64 KiB；输出路径数组/查询数组最多 1024 项，超限返回错误，无部分结果。parse/with_query 可以保留更多参数的原始查询，但 query_pairs 仍受数组限制。

这些小规模纯计算操作同步执行，句柄仅保存拥有型 Rust URL 数据，不保存 Lua 表或字段视图。编码过程可能先生成中间值再检查序列化长度，资源限制不承诺进程 RSS 硬上限。应用可保留多份 URL 值；本模块不涉及网络 I/O、代理、文件路径权限或浏览器业务策略。
