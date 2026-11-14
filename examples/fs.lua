-- 传入一个已存在的临时父目录；示例只操作自己排他创建的子目录。
local fs, path = require('cosmos.fs'), require('cosmos.fs.path')
local parent = (...) or os.getenv('COSMOS_FS_TEST_DIR')
assert(parent, 'pass an existing scratch directory or set COSMOS_FS_TEST_DIR')
assert(fs == cosmos.fs and path == fs.path)
assert(require('cosmos.fs.file') == fs.file and require('cosmos.fs.directory') == fs.directory)
local root = path.join(parent, 'cosmos-fs-' .. string.format('%.0f', cosmos.time.now_us()))
assert(fs.create_dir(root))
local handles = {}
local function open(name, mode)
    local file = assert(fs.open(path.join(root, name), mode))
    handles[#handles + 1] = file
    return file
end
local ok, err = xpcall(function()
    local name = 'binary.dat'
    local filename = assert(path.join(root, name))
    local file = open(name, 'w+x')
    assert(file:write('A\0\255\nB') == 5)
    assert(file:seek(0) == 0)
    assert(file:read(0) == '')
    assert(file:read(3) == 'A\0\255')
    assert(file:read(100) == '\nB')
    assert(file:read() == nil)
    assert(file:seek(-1, 'end') == 4)
    assert(file:read(1) == 'B')
    assert(file:truncate(3))
    assert(file:flush() and file:sync() and file:sync(true))
    local info = assert(file:metadata())
    assert(info.kind == 'file' and info.size == 3 and type(info.modified_ms) == 'number')
    assert(type(info.modified_ns) == 'string')
    assert(cosmos.time.datetime.from_timestamp_ns(info.modified_ns):timestamp_ms() == info.modified_ms)
    assert(file:close() and file:close() and file:is_closed())
    local value, why = file:read()
    assert(value == nil and why == 'closed')
    assert(fs.read_file(filename) == 'A\0\255')
    assert(fs.read_file(filename, 2) == nil)
    assert(fs.open(filename, 'wx') == nil)
    assert(fs.open(filename, 'invalid') == nil)
    assert(fs.open(filename, {write=true, create_new=true}) == nil)
    local appended = open(name, 'a')
    assert(appended:write('tail') == 4 and appended:close())
    assert(fs.read_file(filename) == 'A\0\255tail')
    assert(fs.write_atomic(filename, 'old', {sync=true}))
    assert(fs.write_atomic(filename, 'new', {no_replace=true}) == nil)
    assert(fs.read_file(filename) == 'old')
    local permissions = fs.metadata(filename).mode
    if permissions then assert(fs.chmod(filename, 416)); permissions = 416 end
    assert(fs.write_atomic(filename, 'new'))
    if permissions then assert(fs.metadata(filename).mode == permissions) end
    assert(fs.chmod(filename, -1) == nil and fs.chmod(filename, 4096) == nil)
    assert(fs.copy(filename, path.join(root, 'copy.dat')) == 3)
    assert(fs.rename_new(path.join(root, 'copy.dat'), filename) == nil)
    local moved, move_err = fs.rename_new(path.join(root, 'copy.dat'), path.join(root, 'moved.dat'))
    if not moved then
        assert(move_err:find('unsupported',1,true), move_err)
        assert(fs.rename(path.join(root, 'copy.dat'), path.join(root, 'moved.dat')))
    end
    assert(fs.read_file(path.join(root, 'moved.dat')) == 'new')
    assert(fs.rename(path.join(root, 'moved.dat'), path.join(root, 'renamed.dat')))

    -- 共享文件的写操作串行执行，每一块不会与其他协程交错。
    local shared = open('concurrent.dat', {write=true, read=true, create_new=true})
    local tasks = {}
    for i = 1, 24 do
        tasks[i] = cosmos.thread.spawn(function() assert(shared:write(string.rep(string.char(64+i), 256)) == 256) end)
    end
    for _, task in ipairs(tasks) do assert(task:join() == nil) end
    assert(shared:seek(0) == 0)
    local data = assert(shared:read(8192))
    assert(#data == 24 * 256)
    local seen = {}
    for i = 1, #data, 256 do
        local char = data:sub(i,i)
        assert(data:sub(i,i+255) == string.rep(char,256) and not seen[char])
        seen[char] = true
    end
    assert(shared:settimeout(1000))
    assert(shared:close())

    local locked = open('lock.dat', 'w+')
    local contender = open('lock.dat', 'r+')
    assert(locked:lock())
    local available, timeout = contender:lock(nil, 0)
    assert(available == nil and timeout == 'timeout')
    assert(contender:settimeout(15))
    available, timeout = contender:lock()
    assert(available == nil and timeout == 'timeout' and not contender:is_closed())
    assert(contender:settimeout(0))
    local acquired = cosmos.sync.channel.new(1)
    local waiter = cosmos.thread.spawn(function() assert(contender:lock(nil, 1000)); acquired:send(true) end)
    cosmos.time.sleep(10)
    assert(locked:unlock())
    assert(acquired:recv(1000) and waiter:join() == nil)
    assert(contender:unlock() and contender:unlock())
    assert(contender:close() and locked:close())

    assert(path.filename(filename) == name and path.extension(filename) == 'dat' and path.stem(filename) == 'binary')
    assert(path.parent(filename) == root)
    assert(path.starts_with(filename, root))
    assert(not path.starts_with(root .. '-other/file', root))
    assert(path.strip_prefix(filename, root) == name)
    assert(path.strip_prefix(root .. '-other/file', root) == nil)
    assert(path.is_normal_relative('nested/file') and not path.is_normal_relative('../file'))
    assert(not path.is_normal_relative('') and not path.is_normal_relative(filename))
    assert(not path.is_normal_relative('./file') and not path.is_normal_relative('nested/../file'))
    assert(fs.try_exists(filename) and not fs.try_exists(path.join(root, 'absent')))
    assert(fs.try_exists(path.join(filename, 'child')) == nil)
    assert(path.normalize('a/../../b/./c/..') == '..' .. package.config:sub(1,1) .. 'b')
    assert(path.is_absolute(assert(fs.canonicalize(root))))
    assert(fs.create_dir_all(path.join(root, 'nested/a/b')))
    assert(fs.metadata(path.join(root,'nested')).kind == 'directory')
    local entries = assert(fs.read_dir(root)); handles[#handles+1] = entries
    local found = {}
    while true do local entry = entries:next(); if not entry then break end; found[entry.name] = entry.kind end
    assert(found['binary.dat'] == 'file' and found.nested == 'directory')
    assert(entries:next() == nil and entries:close())
    assert(entries:next() == nil and entries:is_closed())
    assert(fs.remove_dir(path.join(root, 'nested/a/b')))
    assert(fs.remove_dir(path.join(root, 'nested')) == nil)
    assert(fs.remove_dir_all(path.join(root, 'nested')))
    assert(fs.remove_file(filename))
    assert(fs.metadata(filename) == nil)
    assert(fs.read_file('bad\0path') == nil)
    assert(path.normalize('bad\0path') == nil)
    assert(fs.read_file(filename, -1) == nil)
    assert(fs.read_file(filename, 1.5) == nil)
    local probe = open('probe.dat', 'w+')
    assert(probe:read(-1) == nil and probe:read(1.5) == nil)
    assert(probe:seek(-1) == nil and probe:seek(0, 'wrong') == nil)
    assert(probe:truncate(-1) == nil)
    assert(probe:settimeout(math.huge) == nil and probe:settimeout(-1) == nil)
    assert(probe:lock('wrong') == nil)
    assert(probe:close())
end, debug.traceback)
for _, handle in ipairs(handles) do handle:close() end
assert(fs.remove_dir_all(root))
assert(ok, err)
print('Async filesystem, binary data, atomic writes, locks and paths passed')
