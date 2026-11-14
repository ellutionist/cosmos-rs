# cosmos-text

Cosmos 的 Rust 原生文本插件：编译型正则、Unicode 规范化与大小写、OpenCC 繁简转换。模块提供通用操作，名称清洗、匹配规则和转换顺序由 Lua 应用组织。

CLI 默认关闭 text，构建时通过 `--features text` 或 `--features full` 启用。Rust 宿主通过 `machine.add_module(cosmos_text::Text)?` 安装，无需其他插件；需要 Rust 1.89+。完整用法见 [text.lua](../examples/text.lua)。

`require('cosmos.text')` 自动包含 `regex`、`unicode`、`chinese`，子模块的 require 入口与对应字段共用同一张表。合法参数下的操作失败返回 `nil, err`；参数类型不匹配抛 Lua 错误。

## 正则：cosmos.text.regex

使用 Rust `regex` 语法，默认区分大小写、启用 Unicode；不是 Lua pattern 或 PCRE，不支持环视和反向引用。编译后可以复用同一句柄。

| 接口 | 说明 |
| --- | --- |
| `regex.new(pattern[, options])` | 编译并返回正则句柄 |
| `regex.escape(text)` | 转义正则元字符，供动态构造字面量模式 |
| `pattern:pattern()` | 返回原模式 |
| `pattern:capture_names()` | 按序返回 `{index, name}` 数组，包含 index=0 的完整匹配；无名称时 name 缺省 |
| `pattern:is_match(text)` | 是否存在匹配 |
| `pattern:find(text[, start])` | 从一基字节位置搜索，默认 1；无匹配返回 nil |
| `pattern:find_all(text[, limit])` | 返回匹配集合；默认/0 表示全部，正数最多取指定数量 |
| `pattern:replace(text, replacement[, limit])` | 返回新文本、替换次数；默认/0 替换全部，正数限制检查的匹配数量 |
| `pattern:split(text[, max_parts])` | 返回字符串数组；默认/0 返回全部，正数限制段数并在最后一段保留余文 |

编译选项均为布尔值，默认为 false：`case_insensitive`、`multi_line`、`dot_matches_new_line`、`ignore_whitespace`、`swap_greed`。也可在模式中使用 Rust 正则的内联标志。

匹配集合提供 `:len()`、`:get(index)` 和 `:next()`。get 使用一基索引，不推进游标；next 从第一项逐次返回，耗尽后返回 nil。集合内匹配共享原文，保留单个匹配即可安全释放原正则或集合，不引用 Lua 原始字符串。

匹配值提供 `:text()`、`:capture_count()`、`:group([selector])`、`:range([selector])`。selector 为零基捕获组编号或组名，缺省/0 表示完整匹配；未参与匹配的可选组返回 nil，未知名称/越界编号返回 `nil, err`。range 返回两个字节位置，可直接用于 `string.sub(first, last)`。空匹配满足 `last == first - 1`。find 的 start 必须是 UTF-8 字符边界，可以为 `#text + 1`；在完整原文上搜索，保留锚点与单词边界语义。

替换字符串使用 `$0`、`$1`、`${name}` 和 `$$` 模板；未知或未匹配组展开为空，编号/名称默认贪婪识别，拼接后缀时使用 `${1}suffix`。字符串模板统计匹配数。replacement 也可为 Lua 函数，接收匹配值并允许挂起：返回字符串进行替换，返回 nil/false/无返回值保留原匹配；字符串按原文插入，不再次解释模板。返回 `nil, err` 或抛错终止操作，true 及其他类型报错；次数仅统计返回字符串的匹配。回调前先完成有界匹配收集，超限不执行回调；回调自身的副作用无法回滚。

## Unicode：cosmos.text.unicode

| 接口 | 说明 |
| --- | --- |
| `normalize(text, form)` | form 为 `nfc`、`nfd`、`nfkc`、`nfkd` |
| `lower(text)` / `upper(text)` | Rust Unicode 默认大小写映射，保留上下文相关小写规则；不依赖 locale |
| `trim(text)` | 移除两端 Unicode 空白 |
| `len(text)` | Unicode scalar 数量 |
| `truncate(text, max_chars)` | 按 Unicode scalar 截取前缀，0 返回空串 |

len/truncate 不按字节、字形簇或显示宽度计数。例如组合重音字符可能占两个 scalar，规范化为 NFC 后可能成为一个。大小写映射可能改变长度，不等价于 Unicode case folding；模块不隐含规范化、trim 或繁简转换。

## 繁简：cosmos.text.chinese

`chinese.convert(text, config)` 使用 `t2s`（繁转简）或 `s2t`（简转繁）。内嵌词典按方向惰性初始化并缓存，无需安装系统 OpenCC 或分发配置文件。与原后端使用同一 Rust 依赖 `ferrous-opencc 0.4`，支持词组转换，不逐字切块，以保留上下文语义。

转换通常不能无损往返；本阶段只开放这两个通用方向，不加载外部配置，也不附带地区词汇、业务别名或名称聚类策略。实现与内嵌词典来自 [ferrous-opencc](https://github.com/apoint123/ferrous-opencc)，遵循其 Apache-2.0 许可。

## 执行与资源

输入均要求有效 UTF-8，允许 NUL。转换、搜索、编译和字符串模板替换在 Rust 工作线程执行；Lua 参数只在宿主线程准备，替换回调也始终在 Machine 的 LocalSet 上执行。读取匹配/集合元数据是同步操作。每个进程跨 Machine/Runtime 共享最多 2 个运行作业和 16 个已接纳作业（含运行者），超出队列直接返回错误。

- 一般文本输入最多 8 MiB，返回文本最多 16 MiB；OpenCC 输入单独限制为 1 MiB。
- 模式和替换模板最多 64 KiB；模式编译大小限制为 8 MiB、DFA 缓存 2 MiB、嵌套深度 128、捕获槽位最多 256（含完整匹配）。
- 一次集合/替换最多 10000 个匹配、100 万个捕获槽位（含未匹配组）；split 最多 10000 段，超限返回错误，不返回部分结果。显式 limit 可选择前若干项，不提供无限结果隐式截断。
- 已开始的单次原生搜索、字典初始化或 OpenCC 转换不能强制中断。迭代操作在匹配/转换步之间协作检查取消；已取消的原生任务继续持有计算许可直到真正结束，取消排队者会释放等待名额。取消等待者不会发送 Lua 到工作线程。

这些限制约束单次操作和工作并发，不承诺进程 RSS 硬上限。Unicode/OpenCC 内部可能先分配中间结果再检查输出大小，模板展开的字面量最多额外暂存一个模板长度；应用保留的字符串、匹配与多份结果由应用控制。反复正则搜索的复杂度受模式和输入影响，不承诺 find_all/replace 对所有模式都是线性时间。
