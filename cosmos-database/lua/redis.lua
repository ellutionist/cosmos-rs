-- Copyright (C) Yichun Zhang (agentzh)
-- 基于 lua-resty-redis 0.34 适配；原始版权与 BSD 条款见 THIRD_PARTY_NOTICES.md。
local Pool = require "cosmos.database._pool"
local Socket = require "cosmos.network.socket"
local TlsContext = require "cosmos.network.tcp.tls.context"
local _M = require "cosmos.database.redis"
if rawget(_M, "_cosmos_loaded") then return _M end
_M._cosmos_loaded, _M._VERSION, _M.null = true, "0.34-cosmos", cosmos.null
local null = cosmos.null
local unpack, concat = unpack, table.concat
local mt = { __index = _M }
local ok, new_tab = pcall(require, "table.new")
if not ok then new_tab = function() return {} end end
local clear_ok, clear = pcall(require, "table.clear")
if not clear_ok then clear = function(t) for key in pairs(t) do t[key] = nil end end end
local tables = {}
local function request_table() return table.remove(tables) or new_tab(24, 0) end
local function recycle(t)
    clear(t)
    if #tables < 32 then tables[#tables + 1] = t end
end
local function pack(...) return { n = select("#", ...), ... } end
local function valid_ms(value)
    return type(value) == "number" and value >= 0 and value <= 1e12 and value == math.floor(value)
end

local function discard(self)
    local sock = rawget(self, "_sock")
    if sock and not pcall(sock.abort, sock) then return nil, "connection is busy" end
    Pool.finish(self._lease);self._lease=nil
    self._sock, self._connected, self._busy, self._owner = nil, false, false, nil
    self._pending, self._reqs, self._buffered_msg, self._buffered_bytes = 0, nil, nil, 0
    return 1
end

local function invoke(self, operation, ...)
    if self._busy then return nil, "connection is busy" end
    self._busy = true
    local result = pack(pcall(operation, self, ...))
    self._busy = false
    if not result[1] then
        discard(self)
        return nil, "Redis protocol error: " .. tostring(result[2])
    end
    if result[2] == nil and (self._pending or 0) > 0 then discard(self) end
    return unpack(result, 2, result.n)
end

function _M.new()
    return setmetatable({
        _sock = Socket.new(), _connected = false, _pending = 0, _reused = 0,
        _connect_timeout = 5000, _send_timeout = 5000, _read_timeout = 5000,
        _max_reply_size = 64 * 1024 * 1024, _max_request_size = 64 * 1024 * 1024,
        _max_array_len = 65536, _max_depth = 128,
        _subscriptions = { subscribe = {}, psubscribe = {} },
        _subscribed = false, _in_multi = false, _watched = false, _session_dirty = false,
    }, mt)
end

function _M.set_timeouts(self, connect_timeout, send_timeout, read_timeout)
    if self._busy then return nil, "connection is busy" end
    if not valid_ms(connect_timeout) or not valid_ms(send_timeout) or not valid_ms(read_timeout) then
        return nil, "invalid timeout"
    end
    self._connect_timeout, self._send_timeout, self._read_timeout = connect_timeout, send_timeout, read_timeout
    return true
end
function _M.set_timeout(self, timeout) return self:set_timeouts(timeout, timeout, timeout) end
function _M.get_reused_times(self)
    if not self._connected then return nil, "closed" end
    return self._reused
end
function _M.close(self) return discard(self) end

local function options(value, port, extra)
    if type(value) == "string" and (value:sub(1,8) == "redis://" or value:sub(1,9) == "rediss://") then
        return _M.parse_url(value)
    end
    local source = value
    if type(value) == "string" then source = type(port) == "table" and port or extra or {} end
    if type(source) ~= "table" then return nil, "expected Redis options or URL" end
    local copy = {}
    for key, item in pairs(source) do copy[key] = item end
    if type(value) == "string" then
        if value:sub(1,5) == "unix:" then copy.path = value:sub(6)
        else copy.host, copy.port = value, type(port) == "table" and 6379 or port or 6379 end
    end
    return copy
end

local function receive(self, pattern)
    self._sock:settimeout(self._read_timeout)
    return self._sock:receive(pattern)
end
local function charge(budget, count)
    budget.left = budget.left - count
    if budget.left < 0 then error("reply size limit exceeded") end
end
local function count_field(text, limit)
    if text == "-1" then return -1 end
    if not text:match("^%d+$") then error("invalid RESP length") end
    local value = tonumber(text)
    if not value or value > limit then error("RESP length limit exceeded") end
    return value
end
local function integer(text)
    if not text:match("^[+-]?%d+$") then error("invalid RESP integer") end
    local negative = text:sub(1,1) == "-"
    local digits = text:gsub("^[+-]", ""):gsub("^0+", "")
    if digits == "" then return 0 end
    local bound = negative and "9223372036854775808" or "9223372036854775807"
    if #digits > 19 or (#digits == 19 and digits > bound) then error("RESP integer out of range") end
    if #digits < 16 or (#digits == 16 and digits <= "9007199254740991") then return tonumber(text) end
    return (negative and "-" or "") .. digits
end

-- 每个顶层回复独立限制总字节、数组长度和嵌套深度，嵌套错误保留 {false, err}。
local function read_value(self, depth, budget)
    if depth > self._max_depth then error("RESP nesting limit exceeded") end
    local line, err = receive(self, "*L")
    if not line then return nil, err end
    charge(budget, #line)
    if line:sub(-2) ~= "\r\n" then error("invalid RESP line ending") end
    local prefix, value = line:sub(1,1), line:sub(2,-3)
    if prefix == "+" then return value
    elseif prefix == "-" then return false, value
    elseif prefix == ":" then return integer(value)
    elseif prefix == "$" then
        local size = count_field(value, budget.left - 2)
        if size == -1 then return null end
        charge(budget, size + 2)
        local data
        data, err = receive(self, size)
        if not data then return nil, err end
        local ending
        ending, err = receive(self, 2)
        if not ending then return nil, err end
        if ending ~= "\r\n" then error("invalid bulk string terminator") end
        return data
    elseif prefix == "*" then
        local count = count_field(value, self._max_array_len)
        if count == -1 then return null end
        if count > budget.left / 3 then error("reply size limit exceeded") end
        local values = new_tab(count, 0)
        for i=1,count do
            local result, failure = read_value(self, depth + 1, budget)
            if result == nil then return nil, failure end
            values[i] = result == false and {false, failure} or result
        end
        return values
    end
    error("unsupported RESP prefix")
end
local function read_value_root(self, budget) return read_value(self, 1, budget or {left = self._max_reply_size}) end
local function is_message(self, value)
    return self._subscribed and type(value) == "table" and (value[1] == "message" or value[1] == "pmessage")
end
local function message_size(value)
    local size = 0
    for _, field in ipairs(value) do
        if type(field) ~= "string" then error("invalid Pub/Sub message") end
        size = size + #field
    end
    return size
end
local function buffer(self, value)
    local messages = self._buffered_msg or {}
    local size = (self._buffered_bytes or 0) + message_size(value)
    if #messages >= 1024 or size > self._max_reply_size then error("Pub/Sub message buffer limit exceeded") end
    messages[#messages + 1] = value
    self._buffered_msg, self._buffered_bytes = messages, size
end
local function response(self, budget)
    while true do
        local result, err = read_value_root(self, budget)
        if result == nil then return nil, err end
        if is_message(self, result) then buffer(self, result)
        else
            self._pending = self._pending - 1
            return result, err
        end
    end
end

-- 沿用上游 RESP 请求编码和大整数格式规则；socket 发送前拼成一个二进制字符串。
local function encode(cmd, args)
    local request = request_table()
    request[1], request[2], request[3] = "*", args.n + 1, "\r\n"
    local offset = 4
    for i=0,args.n do
        local value = i == 0 and cmd or args[i]
        local kind = type(value)
        if kind == "number" then
            if value ~= value or value == math.huge or value == -math.huge then
                recycle(request); return nil, "Redis arguments must be finite"
            end
            if (value >= 1e14 or value <= -1e14) and value == math.floor(value) and math.abs(value) <= 2^63 then
                value = string.format("%.0f", value)
            else value = tostring(value) end
        elseif kind ~= "string" then
            recycle(request); return nil, "Redis arguments must be strings or numbers"
        end
        request[offset], request[offset+1], request[offset+2] = "$", #value, "\r\n"
        request[offset+3], request[offset+4] = value, "\r\n"
        offset = offset + 5
    end
    local wire = concat(request)
    recycle(request)
    return wire
end
local function send(self, wire, replies)
    if not self._connected then return nil, "closed" end
    if #wire > self._max_request_size then return nil, "request size limit exceeded" end
    self._pending = self._pending + replies
    self._sock:settimeout(self._send_timeout)
    return self._sock:send(wire)
end

local session_commands = { auth=true, select=true, hello=true, readonly=true, readwrite=true, asking=true, reset=true }
local function changes_session(cmd, args)
    if session_commands[cmd] then return true end
    if cmd == "client" then
        local sub = tostring(args[1]):lower()
        return sub == "setname" or sub == "setinfo" or sub == "no-evict" or sub == "no-touch"
    end
    return false
end
local function update_state(self, cmd, result, err, args)
    if cmd == "reset" and result == "RESET" then
        self._subscriptions = {subscribe={}, psubscribe={}}
        self._subscribed, self._in_multi, self._watched, self._transaction_dirty = false, false, false, false
        self._buffered_msg, self._buffered_bytes, self._session_dirty = nil, 0, true
    elseif cmd == "multi" and result == "OK" then
        self._in_multi, self._transaction_dirty = true, false
    elseif (cmd == "exec" or cmd == "discard") and (result ~= false or (err or ""):match("^EXECABORT")) then
        if cmd == "exec" and type(result) == "table" and self._transaction_dirty then self._session_dirty = true end
        self._in_multi, self._watched, self._transaction_dirty = false, false, false
    elseif result ~= false then
        if cmd == "watch" then self._watched = true
        elseif cmd == "unwatch" then self._watched = false end
        if changes_session(cmd, args) and not self._initializing then
            if result == "QUEUED" then self._transaction_dirty = true
            else self._session_dirty = true end
        end
        if cmd == "quit" then discard(self) end
    end
end

local subscriptions = { subscribe="subscribe", psubscribe="psubscribe", unsubscribe="subscribe", punsubscribe="psubscribe" }
local function update_subscription(self, cmd, value)
    if type(value) ~= "table" or #value ~= 3 or value[1] ~= cmd or type(value[3]) ~= "number"
       or value[3] < 0 or (value[2] ~= null and type(value[2]) ~= "string") then
        error("invalid subscription acknowledgement")
    end
    local group = self._subscriptions[subscriptions[cmd]]
    local joining = cmd == "subscribe" or cmd == "psubscribe"
    if value[2] ~= null then group[value[2]] = joining or nil end
    if not joining and self._buffered_msg then
        local retained, bytes = {}, 0
        local kind = cmd == "unsubscribe" and "message" or "pmessage"
        for _, message in ipairs(self._buffered_msg) do
            if message[1] ~= kind or message[2] ~= value[2] then
                retained[#retained+1], bytes = message, bytes + message_size(message)
            end
        end
        self._buffered_msg, self._buffered_bytes = retained, bytes
    end
    self._subscribed = next(self._subscriptions.subscribe) ~= nil or next(self._subscriptions.psubscribe) ~= nil
    if not self._subscribed then self._buffered_msg, self._buffered_bytes = nil, 0 end
end
local function subscription(self, cmd, args)
    if self._reqs or self._in_multi then return nil, "Pub/Sub commands cannot be pipelined or queued in MULTI" end
    local replies = args.n
    if replies == 0 then
        if cmd == "subscribe" or cmd == "psubscribe" then return nil, "subscription requires a channel" end
        replies = 0
        for _ in pairs(self._subscriptions[subscriptions[cmd]]) do replies = replies + 1 end
        replies = math.max(replies, 1)
    end
    local wire, err = encode(cmd, args)
    if not wire then return nil, err end
    local sent
    sent, err = send(self, wire, replies)
    if not sent then return nil, err end
    local results = {}
    for i=1,replies do
        local value, failure = response(self)
        if value == nil then return nil, failure end
        if value == false then self._pending = 0; return false, failure end
        update_subscription(self, cmd, value)
        results[i] = value
    end
    return replies == 1 and results[1] or results
end

local function command(self, cmd, ...)
    if not self._connected then return nil, "closed" end
    if type(cmd) ~= "string" or cmd == "" then return nil, "expected command name" end
    local prefix = rawget(self, "_module_prefix")
    if prefix then cmd, self._module_prefix = prefix .. "." .. cmd, nil end
    local canonical, args = cmd:lower(), pack(...)
    -- 会改变回复协议或进入无界流模式的命令不适用于 RESP2 请求/响应连接。
    if canonical == "monitor" or canonical == "sync" or canonical == "psync"
       or canonical == "ssubscribe" or canonical == "sunsubscribe"
       or (canonical == "hello" and args[1] ~= nil and tostring(args[1]) ~= "2")
       or (canonical == "client" and ({reply=true, tracking=true, caching=true})[tostring(args[1]):lower()]) then
        return nil, "command changes the supported RESP2 reply mode"
    end
    if subscriptions[canonical] then return subscription(self, canonical, args) end
    local wire, err = encode(cmd, args)
    if not wire then return nil, err end
    if #wire > self._max_request_size then return nil, "request size limit exceeded" end
    if self._reqs then
        if #self._reqs >= 10000 or self._pipeline_size + #wire > self._max_request_size then
            return nil, "pipeline size limit exceeded"
        end
        self._reqs[#self._reqs+1] = {wire=wire, cmd=canonical, args=args}
        self._pipeline_size = self._pipeline_size + #wire
        return
    end
    local sent
    sent, err = send(self, wire, 1)
    if not sent then return nil, err end
    local value, failure = response(self)
    if value ~= nil then update_state(self, canonical, value, failure, args) end
    return value, failure
end
function _M.execute(self, cmd, ...) return invoke(self, command, cmd, ...) end

local function connect(self, opts)
    if opts.pool or opts.backlog or opts.tcp_keepalive then return nil, "named pools, backlog and tcp_keepalive are not supported" end
    local timeout = opts.timeout
    local connect_timeout = opts.connect_timeout or timeout or self._connect_timeout
    local send_timeout = opts.send_timeout or timeout or self._send_timeout
    local read_timeout = opts.read_timeout or timeout or self._read_timeout
    if not valid_ms(connect_timeout) or not valid_ms(send_timeout) or not valid_ms(read_timeout) then return nil, "invalid timeout" end
    self._connect_timeout, self._send_timeout, self._read_timeout = connect_timeout, send_timeout, read_timeout
    for name, default in pairs({max_reply_size=64*1024*1024, max_request_size=64*1024*1024, max_array_len=65536, max_depth=128}) do
        local value = opts[name] or default
        if type(value) ~= "number" or value < 1 or value ~= math.floor(value) or value > 1024*1024*1024
           or (name == "max_depth" and value > 256) then return nil, "invalid " .. name end
        self["_" .. name] = value
    end
    local db = opts.db or 0
    if type(db) ~= "number" or db < 0 or db > 2147483647 or db ~= math.floor(db) then return nil, "invalid Redis database number" end
    if opts.username and opts.username ~= "" and opts.password == nil then return nil, "username requires a password" end
    self._reused, self._module_prefix = 0, nil
    self._sock = rawget(self, "_sock") or Socket.new()
    self._sock:settimeout(connect_timeout)
    self._sock:setbufsize(self._max_reply_size)
    local connected, err
    if opts.path then connected, err = self._sock:connect_unix(opts.path)
    else connected, err = self._sock:connect(opts.host or "127.0.0.1", tonumber(opts.port or 6379)) end
    if not connected then discard(conn);return nil, err end
    if opts.ssl or opts.ctx then
        local ctx = opts.ctx
        if not ctx then ctx = TlsContext.new(); if opts.ssl_verify == false then ctx:verify_none() end end
        connected, err = self._sock:starttls(opts.server_name or opts.host or "localhost", ctx)
        if not connected then return nil, "failed to do TLS handshake: " .. err end
    end
    self._connected, self._initializing = true, true
    self._subscriptions = {subscribe={}, psubscribe={}}
    self._subscribed, self._in_multi, self._watched, self._session_dirty = false, false, false, false
    if opts.password ~= nil then
        if opts.username and opts.username ~= "" then connected, err = command(self, "auth", opts.username, opts.password)
        else connected, err = command(self, "auth", opts.password) end
        if not connected then return nil, "failed to authenticate: " .. tostring(err) end
    end
    if db ~= 0 then
        connected, err = command(self, "select", db)
        if not connected then return nil, "failed to select database: " .. tostring(err) end
    end
    self._initializing = false
    return 1
end
function _M.connect(self, value, port, extra)
    if self._busy then return nil, "connection is busy" end
    if self._connected then return nil, "already connected" end
    if self._reqs then return nil, "cancel the pipeline before connecting" end
    local opts, err = options(value, port, extra)
    if not opts then return nil, err end
    local result, failure = invoke(self, connect, opts)
    if not result then discard(self) end
    return result, failure
end

function _M.init_pipeline(self, n)
    if self._busy then return nil, "connection is busy" end
    if not self._connected then return nil, "closed" end
    if self._reqs then return nil, "pipeline already initialized" end
    if self._subscribed then return nil, "subscribed state" end
    if n ~= nil and (type(n) ~= "number" or n < 0 or n > 10000 or n ~= math.floor(n)) then return nil, "invalid pipeline capacity" end
    self._reqs, self._pipeline_size = new_tab(n or 4, 0), 0
    return true
end
function _M.cancel_pipeline(self)
    if self._busy then return nil, "connection is busy" end
    self._reqs, self._pipeline_size = nil, 0
    return true
end
local function commit_pipeline(self)
    local requests = self._reqs
    if not requests then return nil, "no pipeline" end
    self._reqs, self._pipeline_size = nil, 0
    if #requests == 0 then return {} end
    local wires = {}
    for i, request in ipairs(requests) do wires[i] = request.wire end
    local sent, err = send(self, concat(wires), #requests)
    if not sent then return nil, err end
    local results, budget = new_tab(#requests, 0), {left=self._max_reply_size}
    for i, request in ipairs(requests) do
        local result, failure = response(self, budget)
        if result == nil then return nil, failure end
        update_state(self, request.cmd, result, failure, request.args)
        results[i] = result == false and {false, failure} or result
    end
    return results
end
function _M.commit_pipeline(self) return invoke(self, commit_pipeline) end

local function read_reply(self)
    if not self._connected then return nil, "closed" end
    if not self._subscribed then return nil, "not subscribed" end
    local messages = self._buffered_msg
    if messages and #messages > 0 then
        local result = table.remove(messages, 1)
        self._buffered_bytes = self._buffered_bytes - message_size(result)
        return result
    end
    local result, err = read_value_root(self)
    if result == nil then discard(self); return nil, err end
    if not is_message(self, result) then error("expected Pub/Sub message") end
    message_size(result)
    return result
end
function _M.read_reply(self) return invoke(self, read_reply) end

function _M.hmset(self, hash, ...)
    if select("#", ...) == 1 and type((...)) == "table" then
        local fields = {}
        for key, value in pairs((...)) do fields[#fields+1] = key; fields[#fields+1] = value end
        return self:execute("hmset", hash, unpack(fields))
    end
    return self:execute("hmset", hash, ...)
end
function _M.hmget(self, hash, ...)
    if select("#", ...) == 1 and type((...)) == "table" then return self:execute("hmget", hash, unpack((...))) end
    return self:execute("hmget", hash, ...)
end
function _M.array_to_hash(self, value)
    value = value or self
    local result = {}
    for i=1,#value,2 do result[value[i]] = value[i+1] end
    return result
end
local function method(cmd) return function(self, ...) return self:execute(cmd, ...) end end
function _M.add_commands(...)
    for _, cmd in ipairs({...}) do _M[cmd] = method(cmd) end
end
function _M.register_module_prefix(prefix)
    _M[prefix] = function(self) self._module_prefix = prefix; return self end
end

local Client = {}
Client.__index = Client
function _M.client(value, ...)
    if getmetatable(value) == mt then return value:execute("client", ...) end
    local opts, err = options(value)
    if not opts then return nil, err end
    if opts.pool or opts.backlog then return nil, "named pools and backlog are not supported" end
    local size, timeout = opts.pool_size or 10, opts.idle_timeout or 60000
    if type(size) ~= "number" or size < 1 or size > 65536 or size ~= math.floor(size) then return nil, "invalid pool_size" end
    if type(timeout) ~= "number" or timeout < 0 or timeout > 1e12 or timeout ~= timeout then return nil, "invalid idle_timeout" end
    return Pool.attach(setmetatable({_options=opts, _idle={}, _size=size, _idle_timeout=timeout, _closed=false}, Client),opts)
end
function Client:connect()
    local lease,acquire_error=Pool.acquire(self)
    if not lease then return nil,acquire_error end
    while #self._idle > 0 do
        local item = table.remove(self._idle)
        local conn = item.connection
        if (self._idle_timeout == 0 or item.since:elapsed_ms() < self._idle_timeout) and conn._sock:state() == 0 then
            local opts = self._options
            conn:set_timeouts(opts.connect_timeout or opts.timeout or 5000, opts.send_timeout or opts.timeout or 5000, opts.read_timeout or opts.timeout or 5000)
            if conn:ping() == "PONG" then
                if self._closed then discard(conn);Pool.finish(lease);return nil, "client is closed" end
                conn._owner, conn._reused,conn._lease = self, conn._reused+1,lease
                return conn
            end
        end
        discard(conn)
    end
    if self._closed then Pool.finish(lease);return nil,"client is closed" end
    local conn = _M.new()
    conn._lease=lease
    local connected, err = conn:connect(self._options)
    if not connected then discard(conn);Pool.finish(lease);return nil, err end
    if self._closed then discard(conn);Pool.finish(lease);return nil, "client is closed" end
    conn._owner = self
    return conn
end
function Client:release(conn)
    if type(conn)=="table" and conn._scope then return nil,"connection belongs to an active scope" end
    if getmetatable(conn) ~= mt or conn._owner ~= self then return nil, "connection belongs to another client" end
    if conn._busy or conn._pending > 0 or conn._reqs then return nil, "connection has pending operations" end
    if not conn._connected or not conn._sock or conn._sock:state() ~= 0 then discard(conn); return nil, "closed" end
    if conn._subscribed or conn._in_multi or conn._watched or conn._session_dirty then return nil, "connection has subscription, transaction or changed session state" end
    if self._closed then discard(conn);return nil, "client is closed" end
    if #self._idle >= self._size then discard(conn); return 1 end
    local parked = {}
    for key, value in pairs(conn) do parked[key] = value end
    parked._owner,parked._lease = nil,nil
    Pool.finish(conn._lease);conn._lease=nil
    conn._sock, conn._connected, conn._owner = nil, false, nil
    self._idle[#self._idle+1] = {connection=setmetatable(parked, mt), since=cosmos.time.instant.now()}
    return 1
end
function _M.set_keepalive(self)
    if not self._owner then return nil, "connection does not belong to a client" end
    return self._owner:release(self)
end
function Client:close()
    self._closed = true
    Pool.close(self)
    for _, item in ipairs(self._idle) do discard(item.connection) end
    self._idle = {}
end

function Client:stats() return Pool.stats(self) end
function Client:with_connection(callback,...) return Pool.scope(self,callback,nil,...) end
function Client:transaction(callback,...)
    return Pool.scope(self,callback,{
        begin=function(c)return c:multi() end,
        commit=function(c)return c:exec() end,
        rollback=function(c)return c:discard() end,
        return_commit=true,
    },...)
end

setmetatable(_M, {__index=function(_, cmd)
    if type(cmd) ~= "string" or cmd:sub(1,1) == "_" then return nil end
    local fn = method(cmd)
    rawset(_M, cmd, fn)
    return fn
end})
return _M
