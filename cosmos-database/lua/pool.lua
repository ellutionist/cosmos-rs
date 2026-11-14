-- 两种数据库共用租借预算和作用域；不包含协议或业务事务规则。
if package.preload['cosmos.database._pool'] then return end
package.preload['cosmos.database._pool'] = function()
    local Pool = {}
    local function integer(value,min,max) return type(value)=='number' and value==math.floor(value) and value>=min and value<=max end
    local function pack(...) return {n=select('#',...),...} end
    function Pool.attach(client,opts)
        local max=opts.max_connections or opts.pool_size or 10
        if not integer(max,1,65536) then return nil,'invalid max_connections' end
        local wait=opts.max_waiters or math.min(max*4,65536)
        local timeout=opts.acquire_timeout or 5000
        if not integer(wait,0,65536) then return nil,'invalid max_waiters' end
        if not integer(timeout,0,1e12) then return nil,'invalid acquire_timeout' end
        client._size=math.min(client._size,max)
        client._slots=cosmos.sync.semaphore.new(max)
        client._admission=cosmos.sync.semaphore.new(max+wait)
        client._max_connections,client._max_waiters,client._acquire_timeout=max,wait,timeout
        return client
    end
    function Pool.acquire(client)
        if client._closed then return nil,'client is closed' end
        local admission=client._admission:acquire(0)
        if not admission then return nil,client._closed and 'client is closed' or 'connection wait queue is full' end
        local slot,err=client._slots:acquire(client._acquire_timeout)
        if not slot then admission:release();return nil,client._closed and 'client is closed' or ('connection acquire '..err) end
        return {admission=admission,slot=slot}
    end
    function Pool.finish(lease)
        if lease then lease.slot:release();lease.admission:release() end
    end
    function Pool.close(client)
        client._slots:close();client._admission:close()
    end
    function Pool.stats(client)
        local active=client._max_connections-client._slots:available()
        return {in_use=active,waiting=math.max(0,client._max_connections+client._max_waiters-client._admission:available()-active),
            idle=#client._idle,max_connections=client._max_connections,max_waiters=client._max_waiters,closed=client._closed}
    end
    function Pool.scope(client,callback,hooks,...)
        if type(callback)~='function' then return nil,'expected a callback' end
        local conn,err=client:connect();if not conn then return nil,err end
        conn._scope=true
        if hooks then local ok,begin_error=hooks.begin(conn);if not ok then conn:close();return nil,begin_error end end
        local result=pack(pcall(callback,conn,...))
        local failure=not result[1] or (result.n>=3 and (result[2]==nil or result[2]==false) and result[3]~=nil)
        if failure then
            if hooks then pcall(hooks.rollback,conn) end
            conn._scope=nil;conn:close()
            return nil,tostring(result[1] and result[3] or result[2])
        end
        local committed
        if hooks then
            local commit_error;committed,commit_error=hooks.commit(conn)
            if not committed then pcall(hooks.rollback,conn);conn._scope=nil;conn:close();return nil,commit_error or 'transaction commit failed' end
        end
        conn._scope=nil
        local released,release_error=client:release(conn)
        if not released then conn:close();return nil,release_error end
        if hooks and hooks.return_commit then return committed end
        return unpack(result,2,result.n)
    end
    return Pool
end
