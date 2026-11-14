-- 传入临时父目录；文件与 HTTP 均在本地完成，不需要外部服务。
local parent, certs = ...
assert(parent, 'pass an existing scratch directory')
certs = certs or 'examples/certs'
local fs, path = require('cosmos.fs'), require('cosmos.fs.path')
local http = require('cosmos.network.http')
local Body, File = require('cosmos.network.http.body'), require('cosmos.network.http.file')
local thread, time = cosmos.thread, cosmos.time
assert(http.body == Body and http.file == File)
local root = path.join(parent, 'cosmos-http-' .. string.format('%.0f', time.now_us()))
assert(fs.create_dir(root))
local filename, large = path.join(root, 'data'), path.join(root, 'large')
local data = '0123456789\0\255ABCDEF'
assert(fs.write_file(filename, data))
local large_size = 80 * 1024 * 1024 + 7
local sparse = assert(fs.open(large, 'w+'))
assert(sparse:truncate(large_size) and sparse:close())
local handles, servers = {}, {}
local function open(name, mode)
    local f = assert(fs.open(name, mode)); handles[#handles + 1] = f; return f
end
local function source(text, known)
    local at = 1
    return assert(Body.from_callback(function(max)
        if at > #text then return nil end
        local chunk = text:sub(at, at + math.min(max, 3) - 1)
        at = at + #chunk
        return chunk
    end, known and {length = #text} or nil))
end
local generated, last_file, last_input, slow_calls = 0, nil, nil, 0
local function handler(req)
    local route = req:path()
    if route == '/echo' then return {body = assert(req:body())} end
    if route == '/pipe' then return {body = req:stream()} end
    if route == '/callback-pipe' then
        local incoming = req:stream()
        return {body = assert(Body.from_callback(function(max) return incoming:read(max) end))}
    end
    if route == '/sink' then
        local f, input = open(path.join(root, 'upload'), 'w+'), req:stream()
        local count = 0
        while true do
            local chunk, err = input:read(17003)
            if err then f:close(); return {status = 413, body = err} end
            if not chunk then break end
            count = count + #chunk; assert(f:write(chunk) == #chunk)
        end
        assert(f:close())
        return tostring(count)
    end
    if route == '/file' or route == '/strong' or route == '/large' then
        last_file = open(route == '/large' and large or filename)
        return assert(File.response(last_file, req, {
            content_type = 'application/x-test', cache_control = 'private, no-transform',
            etag = route == '/strong' and '"test,v1"' or nil,
        }))
    end
    if route == '/generated' then
        return {body = assert(Body.from_callback(function(max)
            if generated >= large_size then return nil end
            local count = math.min(max, large_size - generated)
            generated = generated + count
            return string.rep('x', count)
        end, {length = large_size}))}
    end
    if route == '/slow' then
        return {body = assert(Body.from_callback(function()
            slow_calls = slow_calls + 1
            time.sleep(1000)
            return 'late'
        end))}
    end
    if route == '/error' then
        return {body = assert(Body.from_callback(function() return nil, 'source failed' end))}
    end
    if route == '/short' then return {body = Body.from_callback(function() return nil end, {length=10})} end
    if route == '/no-content' then
        return {status=204, body=Body.from_callback(function() error('204 must not read') end)}
    end
    if route == '/head-callback' then
        return {body = Body.from_callback(function() error('HEAD must not read') end, {length=123})}
    end
    if route == '/redirect303' or route == '/redirect307' then
        return {status = route == '/redirect303' and 303 or 307, headers={location='/echo'}}
    end
    if route == '/retain-input' then last_input = req:stream(); return 'done' end
    return {status=404}
end
local function listen(options)
    local server = assert(http.server.listen('127.0.0.1', 0, handler, options))
    servers[#servers + 1] = server
    return server
end
local server = listen({stream_body=true, max_body=large_size + 1024})
local base = 'http://127.0.0.1:' .. server:port()
local client = http.client.new()
local function request(method, route, options)
    local response, err = client:request(method, base .. route, options)
    assert(response, err); return response
end
local function settle(predicate)
    for _ = 1, 200 do if predicate() then return end; time.sleep(2) end
    assert(predicate(), 'resource cleanup timed out')
end
local ok, err = xpcall(function()
    assert(request('POST', '/echo', {body=data}).body == data)
    for _, route in ipairs({'/echo', '/pipe', '/callback-pipe'}) do
        for _, known in ipairs({false, true}) do
            local resp = request('POST', route, {body=source(data, known), stream=true})
            local chunks = {}
            while true do
                local chunk, why = resp.body:read(2)
                assert(not why, why); if not chunk then break end
                assert(#chunk <= 2); chunks[#chunks + 1] = chunk
            end
            assert(table.concat(chunks) == data)
            assert(resp.body:is_closed() and next(assert(resp.body:trailers())) == nil)
        end
    end
    print('Streaming binary upload, download and full duplex passed')

    local f = open(filename)
    assert(f:seek(4) == 4)
    local body = assert(Body.from_file(f, {offset=2, length=7}))
    assert(request('POST', '/echo', {body=body}).body == data:sub(3,9))
    settle(function() return body:is_closed() end)
    assert(f:seek(0, 'cur') == 4 and not f:is_closed())
    local reused, why = client:request('POST', base .. '/echo', {body=body})
    assert(reused == nil and why:find('consumed', 1, true), why)
    assert(f:close())
    assert(Body.from_file(open(filename), {offset=-1}) == nil)
    assert(Body.from_file(open(filename), {length=#data+1}) == nil)
    local zero = assert(Body.from_file(open(filename), {length=0, close_file=true}))
    assert(zero:read() == nil)
    assert(Body.from_callback(function() end, {length=1.5}) == nil)
    local consumed = assert(Body.from_file(open(filename)))
    assert(consumed:read(1) == '0')
    assert(client:request('POST', base .. '/echo', {body=consumed}) == nil)
    local truncated = open(path.join(root, 'short-file'), 'w+')
    assert(truncated:write(data) == #data)
    local snapshot = assert(Body.from_file(truncated))
    assert(truncated:truncate(0))
    assert(snapshot:read() == nil and snapshot:is_closed())
    local keep = open(filename)
    local keep_body = assert(Body.from_file(keep, {close_file=false}))
    keep_body:close(); assert(not keep:is_closed())
    assert(keep:close())

    local resp = request('GET', '/file')
    assert(resp.status == 200 and resp.body == data)
    assert(resp.headers['content-type'] == 'application/x-test')
    assert(resp.headers['cache-control'] == 'private, no-transform')
    assert(resp.headers['accept-ranges'] == 'bytes')
    local etag, modified = resp.headers.etag, resp.headers['last-modified']
    assert(etag:sub(1,2) == 'W/' and modified)
    for _, test in ipairs({
        {'bytes=2-5', 206, data:sub(3,6), 'bytes 2-5/' .. #data},
        {'BYTES=2-5', 206, data:sub(3,6), 'bytes 2-5/' .. #data},
        {'bytes=12-', 206, data:sub(13), 'bytes 12-' .. (#data-1) .. '/' .. #data},
        {'bytes=-4', 206, data:sub(-4), 'bytes ' .. (#data-4) .. '-' .. (#data-1) .. '/' .. #data},
        {'bytes=0-9999999999999999999999', 206, data, 'bytes 0-' .. (#data-1) .. '/' .. #data},
        {'bytes=99-', 416, '', 'bytes */' .. #data},
        {'bytes=-0', 416, '', 'bytes */' .. #data},
        {'bytes=7-2', 200, data}, {'bytes=0-1,4-5', 200, data}, {'garbage', 200, data},
    }) do
        local r = request('GET', '/file', {headers={range=test[1]}})
        assert(r.status == test[2] and r.body == test[3], test[1])
        assert(r.headers['content-range'] == test[4])
    end
    assert(request('HEAD', '/file', {headers={range='bytes=1-2'}}).headers['content-length'] == tostring(#data))
    assert(request('HEAD', '/head-callback').headers['content-length'] == '123')
    assert(request('GET', '/no-content').body == '')
    for _, headers in ipairs({{['if-none-match']=etag}, {['if-none-match']='*'}, {['if-modified-since']=modified}}) do
        local r = request('GET', '/file', {headers=headers})
        assert(r.status == 304 and r.body == '')
    end
    assert(request('GET', '/file', {headers={['if-match']=etag}}).status == 412)
    assert(request('GET', '/file', {headers={['if-match']='*'}}).status == 200)
    assert(request('GET', '/file', {headers={['if-unmodified-since']='Thu, 01 Jan 1970 00:00:00 GMT'}}).status == 412)
    assert(request('GET', '/file', {headers={['if-none-match']='"other"', ['if-modified-since']=modified}}).status == 200)
    for _, validator in ipairs({etag, '"other"'}) do
        assert(request('GET', '/file', {headers={range='bytes=0-1', ['if-range']=validator}}).status == 200)
    end
    assert(request('GET', '/strong', {headers={range='bytes=0-1', ['if-range']='"test,v1"'}}).status == 206)
    assert(request('GET', '/strong', {headers={['if-none-match']='"other", W/"test,v1"'}}).status == 304)
    assert(request('GET', '/strong', {headers={['if-match']='"test,v1"'}}).status == 200)
    assert(request('POST', '/file').status == 405)
    settle(function() return last_file:is_closed() end)
    local empty = open(path.join(root, 'empty'), 'w+')
    assert(empty:close())
    local empty_reader = assert(Body.from_file(open(path.join(root, 'empty')), {close_file=true}))
    assert(request('POST', '/echo', {body=empty_reader}).body == '')
    assert(request('GET', '/file', {headers={range='bytes=0-1', ['if-range']=modified}}).status == 200)
    print('File ranges, HEAD and cache preconditions passed')

    -- 稀疏文件超过旧全量读取上限；下载/上传始终只持有一个小块。
    local download = open(path.join(root, 'download'), 'w+')
    local r = request('GET', '/large', {stream=true, timeout=30000})
    local total = 0
    while true do
        local chunk, why = r.body:read(65536)
        assert(not why, why); if not chunk then break end
        total = total + #chunk; assert(download:write(chunk) == #chunk)
    end
    assert(total == large_size and download:metadata().size == large_size and download:close())
    local upload = open(large)
    local upload_body = assert(Body.from_file(upload, {close_file=true}))
    assert(request('POST', '/sink', {body=upload_body, timeout=30000}).body == tostring(large_size))
    settle(function() return upload:is_closed() end)
    assert(fs.metadata(path.join(root, 'upload')).size == large_size)
    print('80 MiB file upload and download passed')

    local limited = request('GET', '/file', {stream=true, max_body=4})
    local value, why = limited.body:read()
    assert(value == nil and why:find('max_body',1,true) and limited.body:is_closed(), why)
    limited = request('GET', '/file', {stream=true})
    value, why = limited.body:read_all(4)
    assert(value == nil and why:find('collection limit',1,true), why)
    local callback = assert(Body.from_callback(function() return '' end))
    value, why = callback:read()
    assert(value == nil and why:find('empty chunk',1,true), why)
    callback = assert(Body.from_callback(function(max) return string.rep('x',max+1) end))
    value, why = callback:read(3)
    assert(value == nil and why:find('exceeds requested size',1,true), why)
    callback = assert(Body.from_callback(function() return nil end, {length=3}))
    value, why = callback:read()
    assert(value == nil and why:find('content-length',1,true), why)
    assert(client:request('GET', base .. '/error', {timeout=1000}) == nil)
    assert(client:request('GET', base .. '/short', {timeout=1000}) == nil)
    assert(request('POST', '/redirect303', {body=source(data)}).body == '')
    value, why = client:request('POST', base .. '/redirect307', {body=source(data), stream=true})
    assert(value == nil and why:find('cannot be replayed',1,true), why)
    assert(request('POST', '/redirect307', {body=data}).body == data)
    print('Limits, source errors and redirect replay rules passed')

    local slow = request('GET', '/slow', {stream=true})
    assert(slow.body:settimeout(10))
    value, why = slow.body:read()
    assert(value == nil and why == 'timeout' and slow.body:is_closed(), why)
    local deadline = request('GET', '/slow', {stream=true, timeout=20})
    time.sleep(25)
    value, why = deadline.body:read()
    assert(value == nil and why == 'timeout', why)
    local expired = request('GET', '/file', {stream=true, timeout=10})
    time.sleep(15)
    value, why = expired.body:read()
    assert(value == nil and why == 'timeout', why)
    local incoming = request('GET', '/slow', {stream=true})
    local reader = thread.spawn(function() incoming.body:read() end)
    time.sleep(5); reader:kill()
    settle(function() return incoming.body:is_closed() end)
    assert(request('GET', '/retain-input').body == 'done')
    settle(function() return last_input:is_closed() end)

    local interrupted = request('GET', '/large', {stream=true})
    interrupted.body:close()
    settle(function() return last_file:is_closed() end)
    generated = 0
    local backpressured = request('GET', '/generated', {stream=true})
    time.sleep(30)
    assert(generated < large_size / 2, 'producer ignored backpressure')
    backpressured.body:close()
    time.sleep(30)
    local stopped = generated
    time.sleep(30)
    assert(generated == stopped, 'producer continued after disconnect')
    print('Timeouts, cancellation, disconnect cleanup and backpressure passed')

    local tls = require('cosmos.network.tcp.tls.context')
    local server_ctx, client_ctx = tls.new_server(), tls.new()
    local function noerr(_, why) assert(why == nil, why) end
    noerr(server_ctx:use_cert_chain_file(path.join(certs, 'cert.pem')))
    noerr(server_ctx:use_private_key_file(path.join(certs, 'key.pem')))
    server_ctx:add_alpn('h2'); server_ctx:add_alpn('http/1.1')
    noerr(client_ctx:add_ca_file(path.join(certs, 'ca.pem')))
    local secure = listen({tls=server_ctx, stream_body=true})
    local secure_client = http.client.new({ctx=client_ctx})
    local response = assert(secure_client:request('POST', 'https://localhost:' .. secure:port() .. '/pipe', {
        body=source(data), stream=true, timeout=3000,
    }))
    assert(response.version == 20 and response.body:read_all() == data)
    print('HTTP/2 streaming over TLS passed')
end, debug.traceback)
for _, server in ipairs(servers) do server:close() end
for _, f in ipairs(handles) do f:close() end
assert(fs.remove_dir_all(root))
assert(ok, err)
print('HTTP streaming passed')
