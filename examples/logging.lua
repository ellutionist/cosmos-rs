-- 需要 logging feature；传入已有临时父目录，不依赖 Encoding/System/Network。
local parent=assert((...),'pass a scratch directory')
local Log,fs,path=cosmos.log,cosmos.fs,cosmos.fs.path
assert(Log==require('cosmos.log'))
local root=assert(path.join(parent,'cosmos-log-'..string.format('%.0f',cosmos.time.now_us())))
assert(fs.create_dir(root))
local filename=assert(path.join(root,'events.log'))
local logger=assert(Log.new({file=filename,console='none',rotation='none',max_bytes=0,level='info'}))
assert(logger:enabled('info') and not logger:enabled('debug'))
assert(logger:debug('filtered')==false)
local child=assert(logger:child({component='demo',request=7}))
local source=debug.getinfo(1,'S').source:sub(2)
local line=debug.getinfo(1,'l').currentline+1
assert(child:info('message with\nnewline',{ok=true,list={1,false,cosmos.null},nested={name='value'}}))
logger:set_level('debug');assert(child:enabled('debug'))
assert(child:debug('debug enabled'))
assert(child:write('warn','explicit level',{value='\228\184\173\230\150\135'}))
assert(logger:flush(true))
local content=assert(fs.read_file(filename))
assert(content:find(source..':'..line,1,true),'Lua source location is missing or wrong')
assert(content:find('component="demo"',1,true) and content:find('request=7',1,true))
assert(content:find('list=[1, false, null]',1,true) and content:find('nested=(name="value")',1,true))
assert(content:find('message with\\nnewline',1,true) and not content:find('filtered',1,true))
assert(content:sub(1,1)~='{' and not content:find('"timestamp":',1,true),'JSON logging is forbidden')
local function bad(value,err) assert(value==nil and type(err)=='string',tostring(err)) end
bad(Log.new({format='json'}))
bad(Log.new({file=filename,console='none',max_bytes=-1}))
bad(logger:write('off','invalid'))
bad(logger:info('\255'))
bad(logger:info('bad',{invalid=0/0}))
bad(logger:info('bad',{invalid=function() end}))
bad(logger:info('bad',{[1]='array root'}))
local cycle={};cycle.self=cycle;bad(logger:info('bad',cycle))
bad(logger:info(string.rep('x',65537)))
bad(logger:info(string.rep('\1',65536)))
assert(logger:info('recovered after invalid input'))
assert(child:close() and logger:close())
bad(logger:info('closed'))
-- 每个事件都带文件和行号；同一 sink 上的并发写入不会交错。
local concurrent_file=assert(path.join(root,'concurrent.log'))
local concurrent=assert(Log.new({file=concurrent_file,console='none',rotation='none',max_bytes=0}))
local tasks={}
for i=1,24 do tasks[i]=cosmos.thread.spawn(function() assert(concurrent:info('concurrent event',{index=i})) end) end
for _,task in ipairs(tasks) do local _,err=task:join();assert(err==nil,err) end
assert(concurrent:close())
local count=0
for entry in assert(fs.read_file(concurrent_file)):gmatch('[^\n]+') do
    count=count+1;assert(entry:find('concurrent event',1,true) and entry:find(source..':',1,true))
end
assert(count==24)
-- 大小轮转保留有限份归档；记录自身不被拆成多行。
local rotating=assert(Log.new({file=path.join(root,'rotation.log'),console='none',rotation='none',max_bytes=1,keep_files=2}))
for i=1,6 do assert(rotating:info('rotation '..i)) end
assert(rotating:close())
assert(fs.metadata(path.join(root,'rotation.log.1')) and fs.metadata(path.join(root,'rotation.log.2')))
assert(fs.try_exists(path.join(root,'rotation.log.3'))==false)
assert(fs.remove_dir_all(root))
print('Text logs, Lua source lines, structured fields, filters, rotation, concurrent writes and flush passed')
