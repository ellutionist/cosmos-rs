-- Network 默认可用；停止接收与排空由应用显式发起。
local certs=(...) or 'examples/certs'
local Server,Client,Body=cosmos.network.http.server,cosmos.network.http.client,cosmos.network.http.body
local Context=cosmos.network.tcp.tls.context
local tls=Context.new_server();tls:use_cert_chain_file(certs..'/cert.pem');tls:use_private_key_file(certs..'/key.pem');tls:add_alpn('h2');tls:add_alpn('http/1.1')
local trust=Context.new();trust:add_ca_file(certs..'/ca.pem')
for _,secure in ipairs({false,true}) do
    for _,force in ipairs({false,true}) do
        local entered=cosmos.sync.channel.new(1)
        local weak=setmetatable({},{__mode='v'})
        local server=assert(Server.listen('127.0.0.1',0,function(req)
            local owner={value='request owner'};weak[1]=owner
            entered:send(true)
            cosmos.time.sleep(force and 60000 or 15)
            assert(owner.value=='request owner')
            local n=0
            return {body=assert(Body.from_callback(function()
                n=n+1;cosmos.time.sleep(2)
                if n<=3 then return 'chunk' end
            end))}
        end,secure and {tls=tls} or nil))
        local client=Client.new(secure and {ctx=trust} or nil)
        local request=cosmos.thread.spawn(function()
            local response,err=client:request('GET',(secure and 'https' or 'http')..'://localhost:'..server:port(),{timeout=2000})
            if force then assert(response==nil or response.status==503,tostring(err))
            else assert(response and response.status==200 and response.body=='chunkchunkchunk',tostring(err));assert(response.version==(secure and 20 or 11)) end
        end)
        assert(entered:recv())
        assert(server:connections()>0)
        assert(server:shutdown(force and 1 or 2000)==not force)
        assert(server:is_draining() and server:connections()==0)
        local _,err=request:join();assert(err==nil,err)
        cosmos.time.sleep(1);collectgarbage('collect');collectgarbage('collect')
        assert(weak[1]==nil,'drained server retained its request callback')
        assert(server:shutdown(0)==true)
        local value,err=client:request('GET',(secure and 'https' or 'http')..'://localhost:'..server:port(),{timeout=100})
        assert(value==nil and type(err)=='string')
    end
end
print('HTTP/1.1 and HTTP/2 graceful drain, streaming, forced shutdown and cleanup passed')
