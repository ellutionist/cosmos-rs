# builtin 时间与日期

所有接口随 Machine 自动安装，通过 `cosmos.time` 或对应 `require` 使用。日期时间和文件系统不需要插件，也没有新增 CLI feature。示例见 [sleep.lua](../examples/sleep.lua) 和 [datetime.lua](../examples/datetime.lua)。

## 系统时间与单调计时

`time.sleep(ms)` 只挂起当前协程，支持小数毫秒；`time.now_ms()` / `time.now_us()` 始终返回真实 Unix 时间戳，会随系统校时改变。

| 接口 | 行为 |
| --- | --- |
| `time.instant.now()` | 创建单调计时句柄 |
| `instant:elapsed_ms()` | 从创建时刻到现在的毫秒数，可带小数 |
| `instant:duration_since(earlier)` | 计算单调时间差，负差返回错误 |
| `instant:deadline(ms)` | 从该 instant 创建截止时间 |
| `time.deadline.after(ms)` | 从现在创建截止时间 |
| `deadline:remaining_ms()` / `:expired()` | 剩余毫秒数（最小 0）/ 是否到期 |
| `deadline:wait()` | 异步等待到期；取消不影响其他使用者 |

时长范围为 0–1e12 ms，拒绝非数、非有限值和负数。单调计时与系统时间戳不能互相混算。

## 日期时间

`local datetime = require('cosmos.time.datetime')`。

| 接口 | 行为 |
| --- | --- |
| `datetime.now(timezone?)` | 当前真实时间，缺省 UTC |
| `datetime.new(parts)` | parts 必须有 year/month/day，可选 hour/minute/second/millisecond/timezone；时间字段缺省 0 |
| `datetime.parse(text, format?, timezone?)` | 缺省解析 RFC3339；自定义格式支持日期或日期时间 |
| `datetime.from_timestamp_ms(ms, timezone?)` | 从精确整数 Unix 毫秒创建 |
| `value:timestamp_ms()` | Unix 毫秒时间戳 |
| `datetime.from_timestamp_ns(text, timezone?)` / `value:timestamp_ns()` | 纳秒十进制整数字符串输入/输出，保留文件 mtime 精度，不经过 Lua number |
| `value:timezone()` / `:with_timezone(name)` | 查询时区 / 创建同一时刻在另一时区的句柄 |
| `value:to_rfc3339()` / `:format(pattern)` | 保留偏移的 RFC3339 / strftime 风格格式化；IANA 时区的 `%Z` 返回对应时刻的缩写 |
| `value:parts()` | year/month/day/hour/minute/second/millisecond/nanosecond、weekday（周一为 1）、ordinal、offset_seconds、timezone |
| `value:add_ms(ms)` / `:diff_ms(earlier)` | 经过时间加减与差值，单位毫秒 |

时区支持 `UTC`、`Z`、`+HH:MM/-HH:MM` 和 IANA 名称（如 `Asia/Shanghai`、`America/New_York`）。IANA 数据随 chrono-tz 编译，不依赖宿主当前时区。

解析含偏移的文本时，偏移确定时刻；额外指定 timezone 只转换显示时区。无偏移的自定义格式按指定时区解释，缺省 UTC。夏令时重叠或跳跃造成的歧义/不存在时间返回错误，需要调用方给出显式偏移，不静默选择其中一个时刻。

句柄不可变。UTC 与显示时区的年份都限制在 1–9999，非法日期、未知时区、越界偏移和闰秒明确报错。输入/格式上限 4096 字节，格式化输出上限 64 KiB。普通错误返回 `nil, err`，参数类型错误抛 Lua 错误，文案为英文。

`add_ms(86400000)` 表示经过 24 小时，DST 边界处不一定是当地日历的下一天。日历运算使用独立的 add_calendar，不混入经过时间 API。


## 日历运算与格式

`value:add_calendar({years=0, months=0, weeks=0, days=0, overflow="clamp", ambiguity="reject"})` 先合并年月，再移动当地日历的周/日，保留当地时分秒与纳秒；各数量必须是有界整数，可以为负。月末默认截断到目标月最后一天（例如 1 月 31 日加一个月得到 2 月 28/29 日），overflow="reject" 时不存在的日期报错。两步运算不保证可逆，年份仍限制为 1..9999。

日历日跨 DST 可能只经过 23/25 小时；不存在的当地时间始终报错，重叠时间默认报错，可用 ambiguity="earliest"/"latest" 显式选择。净移动为零且未指定歧义选择时保留已知的原时刻，不重新解释已有偏移。

`:start_of(unit[, ambiguity])` 返回当地 day/week/month/year 的起点，周一为一周起点；午夜不存在/重叠时遵守相同规则。`:parts()` 另提供 iso_year、iso_week、days_in_month。`datetime.parse_rfc2822(text[, timezone])` 和 `:to_rfc2822()` 提供带偏移的邮件日期格式；RFC3339 与已有 strftime 自定义格式继续有效，所有时钟 now API 仍返回真实当前时间。

实际月末、闰年、DST、跨年 ISO 周、Samoa 跳日及格式验收见 [datetime.lua](../examples/datetime.lua)。没有隐含的节假日、工作日或业务到期规则。
