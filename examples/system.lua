-- 需要 system feature；安装插件本身不会订阅任何信号。
local Process,Signal=cosmos.system.process,cosmos.system.signal
assert(Process==require('cosmos.system.process') and Signal==require('cosmos.system.signal'))
assert(require('cosmos.system')==cosmos.system)
assert(Process.pid()>0 and #assert(Process.executable())>0)
local signal=assert(Signal.subscribe('interrupt'))
local value,err=signal:recv(1)
assert(value==nil and err=='timeout' and not signal:is_closed())
local waiting=cosmos.thread.spawn(function() local value,err=signal:recv();assert(value==nil and err=='closed') end)
local queued=cosmos.thread.spawn(function() local value,err=signal:recv();assert(value==nil and err=='closed') end)
cosmos.time.sleep(1);signal:close();waiting:join();queued:join()
assert(signal:is_closed() and signal:recv(0)==nil)
assert(Signal.subscribe('kill')==nil)
print('Process information and explicit signal timeout, cancellation and close passed')
