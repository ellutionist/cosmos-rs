-- 从环境变量或脚本参数读取连接 URL，不在仓库中保存数据库凭据。
local mysql = require("cosmos.database.mysql")
local url = (...) or os.getenv("COSMOS_MYSQL_URL")
assert(url, "请设置 COSMOS_MYSQL_URL 或传入 mysql:// URL")
assert(require("cosmos.database").mysql == mysql and mysql.null == cosmos.null)
local opts = assert(mysql.parse_url(url))
opts.timeout, opts.pool_size, opts.idle_timeout = 2000, 2, 60000
local client = assert(mysql.client(opts))
local db = assert(client:connect())
assert(db:get_reused_times() == 0)
local info = assert(db:query("SELECT VERSION() AS version, DATABASE() AS name, CONNECTION_ID() AS id"))[1]
assert(info.name == opts.database)
local id = info.id
local row = assert(db:query("SELECT NULL AS null_value, CAST(18446744073709551615 AS UNSIGNED) AS big, '中文' AS text"))[1]
assert(row.null_value == cosmos.null and row.big == "18446744073709551615" and row.text == "中文")
local binary = "引号'反斜线\\换行\n\0\255"
assert(assert(db:query("SELECT " .. mysql.quote_sql_str(binary) .. " AS value"))[1].value == binary)

-- 只创建会话临时表，不修改现有业务表。
assert(db:query("CREATE TEMPORARY TABLE cosmos_mysql_smoke (id INT PRIMARY KEY, value VARBINARY(256))"))
assert(db:query("INSERT INTO cosmos_mysql_smoke VALUES (1, " .. mysql.quote_sql_str(binary) .. ")").affected_rows == 1)
assert(assert(db:query("SELECT value FROM cosmos_mysql_smoke WHERE id=1"))[1].value == binary)
assert(db:query("BEGIN"))
assert(db:query("INSERT INTO cosmos_mysql_smoke VALUES (2, X'00')"))
local pooled, pool_err = db:set_keepalive()
assert(pooled == nil and pool_err:find("transaction", 1, true))
assert(db:query("ROLLBACK"))
assert(tonumber(assert(db:query("SELECT COUNT(*) AS n FROM cosmos_mysql_smoke"))[1].n) == 1)
assert(db:query("DROP TEMPORARY TABLE cosmos_mysql_smoke"))
local result, err, errno, sqlstate = db:query("SELECT FROM")
assert(result == nil and err and errno == 1064 and sqlstate == "42000")
assert(db:ping())

-- 多结果集须逐个读完，读取期间不得回池。
local first, again = db:query("SELECT 1 AS n; SELECT 2 AS n")
assert(tonumber(first[1].n) == 1 and again == "again")
assert(db:set_keepalive() == nil)
assert(tonumber(db:read_result()[1].n) == 2)
assert(db:send_query("SELECT 3 AS n"))
assert(client:release(db) == nil)
assert(tonumber(db:read_result()[1].n) == 3)
assert(db:set_keepalive())
assert(db:query("SELECT 99") == nil, "归还后的旧句柄必须失效")
local reused = assert(client:connect())
assert(reused:get_reused_times() == 1)
assert(assert(reused:query("SELECT CONNECTION_ID() AS id"))[1].id == id)
reused:set_compact_arrays(true)
local compact = assert(reused:query("SELECT NULL, 42"))
assert(#compact[1] == 2 and compact[1][1] == cosmos.null and tonumber(compact[1][2]) == 42)
assert(client:release(reused))

-- 不同 client 不共享连接池。
local other = assert(mysql.client(opts))
local separate = assert(other:connect())
assert(assert(separate:query("SELECT CONNECTION_ID() AS id"))[1].id ~= id)
assert(separate:close())
other:close()

-- 只终止自己创建并归还的测试连接，验证租借时发现失效连接并重建。
local stale = assert(client:connect())
local stale_id = assert(stale:query("SELECT CONNECTION_ID() AS id"))[1].id
assert(client:release(stale))
local control = mysql.new()
assert(control:connect(opts))
assert(control:query("KILL CONNECTION " .. stale_id))
assert(control:close())
local recovered = assert(client:connect())
assert(recovered:get_reused_times() == 0)
assert(assert(recovered:query("SELECT CONNECTION_ID() AS id"))[1].id ~= stale_id)
assert(client:release(recovered))

-- 读超时和取消后的连接不能回池。
local timed = assert(client:connect())
assert(timed:set_timeout(10))
local _, timeout = timed:query("SELECT SLEEP(0.1)")
assert(timeout and timeout:find("timeout", 1, true))
assert(client:release(timed) == nil)
local cancelled = assert(client:connect())
local started = false
local task = cosmos.thread.spawn(function()
    started = true
    cancelled:query("SELECT SLEEP(0.1)")
end)
while not started do cosmos.time.sleep(0) end
cosmos.time.sleep(10)
task:kill()
local _, cancellation = task:join()
assert(cancellation and cancellation:find("cancelled", 1, true))
assert(client:release(cancelled) == nil)
assert(cancelled:close())

local tasks = {}
for i=1,4 do
    tasks[i] = cosmos.thread.spawn(function()
        local connection = assert(client:connect())
        assert(tonumber(assert(connection:query("SELECT SLEEP(0.001) AS value"))[1].value) == 0)
        assert(client:release(connection))
    end)
end
for _, pending in ipairs(tasks) do
    local _, failure = pending:join()
    assert(failure == nil, failure)
end
client:close()
assert(client:connect() == nil)

local expiry_options = assert(mysql.parse_url(url))
expiry_options.idle_timeout = 5
local expiring = assert(mysql.client(expiry_options))
local before = assert(expiring:connect())
local before_id = assert(before:query("SELECT CONNECTION_ID() AS id"))[1].id
assert(expiring:release(before))
cosmos.time.sleep(10)
local after = assert(expiring:connect())
assert(after:get_reused_times() == 0)
assert(assert(after:query("SELECT CONNECTION_ID() AS id"))[1].id ~= before_id)
assert(after:close())
expiring:close()
print("MySQL 认证、查询、NULL、大整数、多结果集、事务、连接池、超时与取消验证通过：", info.version, info.name)
