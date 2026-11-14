# XML

`cosmos.encoding.xml` 提供独立 XML 文档与节点模型，支持 UTF-8 XML 1.0 和命名空间；不经过 JSON，不把重复元素折叠成对象字段。它随 Encoding 插件安装，CLI 沿用 `encoding` feature。

完整用法见 [xml.lua](../examples/xml.lua)：读取文档、构造混合内容、修改指定节点、命名空间迁移和 FS 原子写回。可用 `cosmos-cli examples/xml.lua /tmp` 运行，参数是已存在的临时父目录；省略参数跳过文件写回部分。

## 读取

`xml.parse(text)` 返回文档，非法 XML 返回 `nil, err`。输入必须是有效 UTF-8；XML 声明仅接受 1.0 和 UTF-8（大小写不限），支持 UTF-8 BOM。完整文档必须有一个根元素。

| 接口 | 语义 |
| --- | --- |
| `document:root()` | 根元素节点 |
| `document:children()` | 按原文顺序返回文档级节点；声明不作为节点 |
| `document:encode()` | 返回完整原文，保留 BOM、声明、空白、引号和属性顺序 |
| `node:kind()` | `element` / `text` / `cdata` / `comment` / `pi` |
| `node:name()` / `:local_name()` / `:prefix()` / `:namespace()` | 限定名、本地名、前缀、命名空间 URI；不适用或没有值时返回 nil；PI 的 name 是 target |
| `node:children()` / `:descendants()` | 有序子节点 / 深度优先后代，不包含节点自身 |
| `node:elements(name?, namespace?)` | 有序直接子元素；无参数返回全部；提供本地名时默认匹配无命名空间，namespace 可显式指定 URI |
| `node:child(name, namespace?)` | 同上，返回第一个匹配的子元素或 nil |
| `node:attributes()` | 有序属性数组，每项含 `name, local_name, prefix, namespace, value`；xmlns 声明单独由 namespaces 返回 |
| `node:attribute(name, namespace?)` | 按本地名和 URI 读取属性；默认无命名空间，无匹配为 nil |
| `node:namespaces()` | 当前可见的前缀绑定数组，每项为 `{prefix, uri}`；默认前缀是空串，包含隐含 xml 绑定 |
| `node:text()` | 元素的全部后代文本和 CDATA 连接；文本/CDATA 为解码值，注释/PI 为自身内容 |
| `node:raw()` | 该节点的原始字节片段，不补命名空间声明 |
| `node:range()` | 在所属解析文档中的 `first, last` 字节位置，可直接用于 Lua `string.sub`；构造节点返回 nil |
| `node:encode()` | 序列化为独立片段，为元素补齐继承的命名空间声明 |

CDATA 与相邻文本保持独立节点，注释和 PI 也保留顺序。文本解码 XML 的五种内建实体及数字字符引用，按 XML 1.0 规则处理换行与属性空白；不自动 trim，也不把内容转成数字/布尔值。原文仍保留原始字符引用和 CDATA 写法。

默认命名空间不作用于无前缀属性。无命名空间的元素/属性，其 namespace 返回 nil；`xmlns=""` 表示清除默认命名空间。查询命名空间 URI 使用字符串精确比较，遵循 [XML Namespaces](https://www.w3.org/TR/xml-names/)。

## 构造

- `xml.element(name, attributes?, children?)` 创建元素。name 是限定名；attributes 是有序的 `{name, value}` 数组，命名空间通过 `xmlns` / `xmlns:p` 属性声明。children 是有序数组，可混合普通字符串与 XML 节点；字符串作为文本自动转义。
- `xml.text(text)`、`xml.cdata(text)`、`xml.comment(text)`、`xml.pi(target, text)` 创建对应节点。CDATA 不接受 `]]>`，注释不接受非法 `--`，PI target 必须是非保留 NCName，内容不能包含 `?>`；文本自动转义。
- `xml.document.new(root)` 将一个元素作为根，创建完整文档。节点可先作为片段构造，再组成文档；每个元素片段自身必须有完整的命名空间绑定。

例如有序混合内容可写成 `xml.element('p', nil, {'before ', xml.element('b', nil, {'bold'}), ' after'})`。构造后的名称、属性、字符和命名空间同样接受完整校验，不允许把原始标记混入 name 或属性名。

## 修改与原文保留

`document:replace(node, replacement?)` 替换指定节点；replacement 省略或为 nil 表示删除。`document:insert(parent, child, index?)` 在元素子节点序列中插入节点；index 从 1 开始，默认追加，空元素会自动展开为起止标签。删除唯一根元素等操作若产生非法文档，会返回错误。

编辑返回新文档，旧文档和旧节点仍可读取。编辑新版本时须从新文档获取节点，使用旧版本或其他文档的节点定位会返回错误。多个目标可依次查询并修改；没有可变共享 DOM、父子引用环或悬空的解析器借用。

替换/插入只改目标字节区域，再校验新文档；其余原文逐字节保留。跨文档插入、替换及元素片段序列化会按需补齐命名空间声明，包括无命名空间元素进入默认命名空间时补 `xmlns=""`，避免改变元素或属性的命名空间含义。`raw()` 保留原文，单独搬运时应使用 `encode()` 或节点编辑 API。

FS 读写与原子发布由 `cosmos.fs` 组合；库不内置 NFO 字段、路径访问规则、备份策略或业务文本修复。严格解析会拒绝未转义的 `&`；是否修复既有不规范文件由应用决定。

## 资源与异步

- 输入与输出各最多 16 MiB，最多 64 层元素、100000 个逻辑节点、100000 个属性/命名空间声明。元素/属性构造名称最多 1024 字节；有序构造序列也受 16 MiB 原始数据预算和节点数限制。
- URI 在内部共享，避免每个后代复制巨大的继承 URI；属性与命名空间列表在生成 Lua 输出前另检查展开后的 16 MiB 预算。序列拒绝空洞、非整数键、混合键和无效节点类型。
- DTD（包括空 DTD）、自定义/外部实体均拒绝，不加载文件或网络资源。不提供 XSD/DTD 校验、XPath 或 XInclude 执行。
- 解析、构造、片段序列化与编辑在 Rust 阻塞工作线程执行；元数据查询和文档原文输出同步执行。Lua 参数在宿主线程准备，工作线程只接收拥有型 Rust 数据。异步接口应在可挂起的 Lua 协程中执行；require 初始化阶段可保留模板字符串，进入执行阶段后再解析。
- XML 在所有 Machine/Runtime 之间共享最多 2 个原生作业、16 个已接纳作业（包括运行中）的额度；队列满返回 `nil, "XML worker queue is full"`。取消可撤销未开始的任务，已运行的解析不能强制中断，完成后丢弃结果并回收资源，期间继续占用额度。

节点独立保活其所属原文，释放文档变量不会使节点失效。保留旧版本节点也会保留旧版原文；应用不再需要时释放句柄即可。类型错误沿用桥接层的 Lua 错误；文档、构造序列与编辑内容不合法时返回 `nil, err`。

原生结构与命名空间验证使用 [roxmltree](https://docs.rs/roxmltree/0.21.2/roxmltree/)，词法边界与预检使用 [xmlparser](https://docs.rs/xmlparser/0.13.6/xmlparser/)，均为 Rust 实现。
