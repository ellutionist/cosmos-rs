-- 用自己的 HttpClient 压自己的 HttpServer
--
-- 服务端：cargo run --release -- examples/http_bench.lua <port> [body_bytes]
-- 客户端：cargo run --release -- examples/http_bench_client.lua <port> [conns] [seconds] [path] [method] [body_bytes]
--
-- 每个 worker 是一个 Lua 协程：发一个请求、等响应、再发下一个，
-- N 个 worker 并发即 N 条 keep-alive 连接（与 wrk -c N 等价的负载形态）。

io.stdout:setvbuf("no")

local HttpClient = require("cosmos.network.http.client")
local client = HttpClient.new()

local a_port, a_conns, a_secs, a_path, a_method, a_body = ...
local port = tonumber(a_port) or 18090
local conns = tonumber(a_conns) or 10
local seconds = tonumber(a_secs) or 10
local path = a_path or "/"
local method = a_method or "GET"
local body = a_body and string.rep("x", tonumber(a_body)) or nil
local opts = body and { body = body } or nil
local url = string.format("http://127.0.0.1:%d%s", port, path)

-- 延迟直方图：µs 上界，最后一档为 >10ms
local BOUNDS = { 30, 50, 60, 70, 80, 90, 100, 120, 150, 200, 300, 500, 1000, 2000, 5000, 10000 }

local function bucket_of(us)
  for i = 1, #BOUNDS do
    if us <= BOUNDS[i] then
      return i
    end
  end
  return #BOUNDS + 1
end

local function run_worker(deadline)
  local n, errs, sum, max_us, bytes = 0, 0, 0.0, 0.0, 0
  local hist = {}
  local last_err = nil
  local t_start = cosmos.time.now_us()
  while cosmos.time.now_ms() < deadline do
    local t0 = cosmos.time.now_us()
    local r, e = client:request(method, url, opts)
    local dt = cosmos.time.now_us() - t0
    if r and r.status == 200 then
      n = n + 1
      sum = sum + dt
      if dt > max_us then
        max_us = dt
      end
      bytes = bytes + #r.body
      local b = bucket_of(dt)
      hist[b] = (hist[b] or 0) + 1
    else
      errs = errs + 1
      last_err = e or (r and ("status " .. r.status)) or "unknown"
    end
  end
  return {
    n = n,
    errs = errs,
    sum = sum,
    max = max_us,
    bytes = bytes,
    hist = hist,
    last_err = last_err,
    t_start = t_start,
    t_end = cosmos.time.now_us(),
  }
end

-- 预热：建立连接 + 触发 JIT
assert(client:request(method, url, opts))

local deadline = cosmos.time.now_ms() + seconds * 1000
local results = {}
local threads = {}
for i = 1, conns do
  local idx = i
  threads[i] = cosmos.thread.spawn(function()
    results[idx] = run_worker(deadline)
  end)
end
for i = 1, conns do
  local _, err = threads[i]:join()
  if err then
    error(string.format("worker %d failed: %s", i, err))
  end
end

local total = { n = 0, errs = 0, sum = 0.0, max = 0.0, bytes = 0 }
local hist = {}
local t_first, t_last = math.huge, 0
local last_err = nil
for i = 1, conns do
  local r = assert(results[i], "missing worker result")
  total.n = total.n + r.n
  total.errs = total.errs + r.errs
  total.sum = total.sum + r.sum
  total.max = math.max(total.max, r.max)
  total.bytes = total.bytes + r.bytes
  if r.t_start < t_first then t_first = r.t_start end
  if r.t_end > t_last then t_last = r.t_end end
  last_err = last_err or r.last_err
  for b, c in pairs(r.hist) do
    hist[b] = (hist[b] or 0) + c
  end
end

local elapsed = (t_last - t_first) / 1e6
local n = total.n

local function percentile(p)
  if n == 0 then
    return 0
  end
  local target = n * p / 100
  local seen = 0
  for b = 1, #BOUNDS + 1 do
    seen = seen + (hist[b] or 0)
    if seen >= target then
      return BOUNDS[b] or BOUNDS[#BOUNDS]
    end
  end
  return BOUNDS[#BOUNDS]
end

print(string.format("conns=%d window=%.2fs method=%s path=%s", conns, elapsed, method, path))
print(string.format("requests=%d errors=%d", n, total.errs))
if n > 0 then
  print(string.format("rps=%.0f", n / elapsed))
  print(string.format(
    "latency avg=%.1fus p50<=%dus p90<=%dus p99<=%dus max=%.1fus",
    total.sum / n,
    percentile(50),
    percentile(90),
    percentile(99),
    total.max
  ))
  print(string.format("throughput=%.2fMB/s", total.bytes / elapsed / (1024 * 1024)))
end
if total.errs > 0 then
  print("last error: " .. tostring(last_err))
end
