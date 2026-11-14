-- 增量摘要、HMAC、密码和随机数；传入临时父目录，不依赖 Encoding 插件。
local parent = assert((...), 'pass an existing scratch directory')
local e = require('cosmos.encryption')
local Digest, Hmac = require('cosmos.encryption.digest'), require('cosmos.encryption.hmac')
local Password, Random = require('cosmos.encryption.password'), require('cosmos.encryption.random')
for _, name in ipairs({'digest','hmac','password','random'}) do
    package.loaded['cosmos.encryption.' .. name]=nil
    assert(require('cosmos.encryption.' .. name) == e[name])
end
local fs, path = cosmos.fs, cosmos.fs.path
assert(e.digest == Digest and e.hmac == Hmac and e.password == Password and e.random == Random)
local function hex(data) return (data:gsub('.', function(char) return string.format('%02x', char:byte()) end)) end
-- require 初始化经过不可挂起的 C 调用边界，旧摘要入口必须继续同步可用。
package.preload['cosmos_test_sync_digest'] = function()
    return {e.sha1('abc'),e.sha256('abc'),e.sha512('abc')}
end
local module = require('cosmos_test_sync_digest')
assert(module[1] == e.sha1('abc') and module[2] == e.sha256('abc') and module[3] == e.sha512('abc'))
local function verifier(algorithm, key, data, tag)
    local ctx = assert(Hmac.new(algorithm,key))
    assert(ctx:update(data))
    return ctx:verify(tag)
end
for _, algorithm in ipairs({'sha1','sha256','sha512'}) do
    local ctx = assert(Digest.new(algorithm))
    assert(ctx:update('a') and ctx:update('') and ctx:update('b\0\255'))
    local expected = assert(e[algorithm]('ab\0\255'))
    assert(ctx:finalize() == expected and ctx:is_closed())
    assert(ctx:finalize() == nil and ctx:update('x') == nil)
    assert(Digest.sum(algorithm,'ab\0\255') == expected)
    local valid = assert(Digest.new(algorithm)); assert(valid:update('ab\0\255'))
    assert(valid:verify(expected) and valid:is_closed())
    local invalid = assert(Digest.new(algorithm)); assert(invalid:verify('') == false)
    local empty = assert(Digest.new(algorithm))
    assert(empty:finalize() == e[algorithm](''))
end
assert(Digest.new('invalid') == nil)
local closed = assert(Digest.new('sha256'))
assert(closed:close() and closed:close() and closed:is_closed() and closed:update('') == nil)

-- RFC 2202 / 4231 的第一组 HMAC 向量。
local tags = {
    sha1='b617318655057264e28bc0b6fb378c8ef146be00',
    sha256='b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7',
    sha512='87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854',
}
for algorithm, tag in pairs(tags) do
    local key = string.rep('\x0b',20)
    local mac = assert(Hmac.new(algorithm,key))
    assert(mac:update('Hi ') and mac:update('There'))
    local signature = assert(mac:finalize())
    assert(hex(signature) == tag and mac:is_closed())
    assert(Hmac.sum(algorithm,key,'Hi There') == signature)
    assert(verifier(algorithm,key,'Hi There',signature))
    assert(verifier(algorithm,key,'Hi There!',signature) == false)
    assert(verifier(algorithm,key,'Hi There',signature:sub(2)) == false)
    assert(verifier(algorithm,key,'Hi There',signature .. 'x') == false)
    local binary = assert(Hmac.sum(algorithm,'key\0\255','data\0\255'))
    assert(verifier(algorithm,'key\0\255','data\0\255',binary))
    assert(Hmac.new(algorithm,string.rep('x',65537)) == nil)
end
assert(Hmac.new('invalid','key') == nil)
local oversized=string.rep('x',64*1024*1024+1)
local intact=assert(Digest.new('sha256'))
assert(intact:update(oversized) == nil and not intact:is_closed())
assert(Digest.sum('sha256',oversized) == nil)
assert(Hmac.sum('sha256','key',oversized) == nil)
oversized=nil; collectgarbage('collect')
assert(intact:finalize() == e.sha256(''))
print('Incremental digests, binary input and HMAC standard vectors passed')

local root = path.join(parent,'cosmos-crypto-' .. string.format('%.0f',cosmos.time.now_us()))
assert(fs.create_dir(root))
local file
local ok, err = xpcall(function()
    file = assert(fs.open(path.join(root,'data'),'w+x'))
    local chunk = string.rep('a\0\255B',16384)
    local expected = assert(Digest.new('sha256'))
    for _=1,128 do assert(file:write(chunk) == #chunk); assert(expected:update(chunk)) end
    assert(file:seek(0) == 0)
    local digest = assert(Digest.new('sha256'))
    while true do
        local data, why = file:read(17001)
        assert(not why,why); if not data then break end
        assert(digest:update(data))
    end
    assert(digest:verify(assert(expected:finalize())))
    assert(file:close()); file=nil
    print('Incremental file checksum passed')

    -- 默认参数与原后端相同；密码策略保留给应用，不限制文本编码或是否为空。
    local ticks, ticking = 0, true
    local ticker = cosmos.thread.spawn(function()
        while ticking do cosmos.time.sleep(1); if ticking then ticks=ticks+1 end end
    end)
    local encoded = assert(Password.hash('pass\0\255word'))
    ticking=false; assert(ticker:join() == nil and ticks > 0, 'password job blocked the Lua thread')
    assert(encoded:find('$argon2id$v=19$m=19456,t=2,p=1$',1,true) == 1)
    assert(Password.verify('pass\0\255word',encoded))
    assert(Password.verify('wrong',encoded) == false)
    local another = assert(Password.hash('pass\0\255word'))
    assert(another ~= encoded and Password.verify('pass\0\255word',another))
    local small = {memory_kib=32,iterations=1,parallelism=1,output_length=16}
    assert(Password.verify('',assert(Password.hash('',small))))
    for _, options in ipairs({
        {memory_kib=65537},{iterations=11},{parallelism=5},{output_length=65},
        {memory_kib=8,parallelism=2},{iterations=1.5},{memory_kib=math.huge},
    }) do assert(Password.hash('test',options) == nil) end
    assert(Password.hash(string.rep('p',4097)) == nil)
    for _, phc in ipairs({
        'invalid', '$argon2id$v=19$m=65537,t=2,p=1$c29tZXNhbHQ$YWJjZGVmZ2hpamtsbW5vcA',
        '$argon2id$v=19$m=32,t=11,p=1$c29tZXNhbHQ$YWJjZGVmZ2hpamtsbW5vcA',
        '$argon2id$v=19$m=32,t=2,p=1$c29tZXNhbHQ', string.rep('x',513),
    }) do assert(Password.verify('test',phc) == nil) end

    local progress = 0
    local timer = cosmos.thread.spawn(function()
        for _=1,20 do cosmos.time.sleep(1); progress=progress+1 end
    end)
    local workers = {}
    for i=1,4 do workers[i]=cosmos.thread.spawn(function()
        assert(Password.verify('concurrent',assert(Password.hash('concurrent',{memory_kib=1024,iterations=2}))))
    end) end
    for _, worker in ipairs(workers) do assert(worker:join() == nil) end
    assert(timer:join() == nil and progress == 20)
    print('Argon2 PHC, random salts, validation and asynchronous password jobs passed')

    assert(Random.bytes(0) == '')
    assert(#assert(Random.bytes(32)) == 32 and Random.bytes(32) ~= Random.bytes(32))
    for _, count in ipairs({-1,1.5,1048577,math.huge}) do assert(Random.bytes(count) == nil) end
        for _=1,64 do
        local n = assert(Random.integer(-3,3))
        assert(n % 1 == 0 and n >= -3 and n <= 3)
    end
    assert(Random.integer(7,7) == 7)
    for _, pair in ipairs({{-9007199254740991,9007199254740991},{9007199254740989,9007199254740991}}) do
        local n = assert(Random.integer(pair[1],pair[2])); assert(n >= pair[1] and n <= pair[2] and n % 1 == 0)
    end
    assert(Random.integer(2,1) == nil and Random.integer(0,1.5) == nil)
    assert(Random.integer(0,9007199254740992) == nil)
    print('OS random bytes and exact integer ranges passed')
end,debug.traceback)
if file then file:close() end
assert(fs.remove_dir_all(root))
assert(ok,err)
