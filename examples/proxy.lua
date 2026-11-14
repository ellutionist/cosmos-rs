-- 本地 HTTP 正向代理验收；直连规则由应用回调逐跳决定。
local Proxy, Client, Server = cosmos.network.proxy, cosmos.network.http.client, cosmos.network.http.server
local Url = cosmos.network.http.url
assert(Proxy == require('cosmos.network.proxy'))
local function bad(value, err) assert(value==nil and type(err)=='string',tostring(err)) end
for _, url in ipairs({'ftp://localhost', 'http://localhost/path','http://localhost/?q=x',
    'http://localhost/#fragment','http://localhost:0','socks5://user@localhost','http://u%3As:p@localhost'}) do
    local value, err=Proxy.new(url);bad(value,err)
    assert(not err:find('secret',1,true),'proxy diagnostics leaked credentials')
end
bad(Proxy.new('\255'))
assert(Proxy.new('socks5://localhost'):port()==1080)
assert(Proxy.new('https://localhost'):port()==443)
local origin=assert(Server.listen('127.0.0.1',0,function(req)
    assert(req:header('proxy-authorization')==nil,'proxy credentials reached origin')
    if req:path()=='/redirect' then return {status=302,headers={location='/bypass'}} end
    return req:target() .. ' ' .. assert(req:body())
end))
local direct=assert(Client.new())
local proxy_requests, peers=0,{}
local forward=assert(Server.listen('127.0.0.1',0,function(req)
    proxy_requests=proxy_requests+1;peers[#peers+1]=req:remote_addr()
    assert(req:header('proxy-authorization')=='Basic dXNlcjpzZWNyZXQ=')
    local target='http://'..assert(req:header('host'))..req:target()
    assert(Url.parse(target):host()=='127.0.0.1')
    local response=assert(direct:request(req:method(),target,{body=assert(req:body()),redirects=0}))
    return {status=response.status,body=response.body,headers={['x-proxy']='visited',location=response.headers.location}}
end))
local proxy=assert(Proxy.new('http://user:secret@127.0.0.1:'..forward:port()))
assert(proxy:scheme()=='http' and proxy:host()=='127.0.0.1' and proxy:port()==forward:port())
local decisions={}
local client=assert(Client.new({proxy=function(target)
    cosmos.time.sleep(1)
    local url=assert(Url.parse(target));decisions[#decisions+1]=url:path()
    if url:path()=='/bypass' then return nil end
    return proxy
end}))
local root='http://127.0.0.1:'..origin:port()
local response=assert(client:request('POST',root..'/via?x=1',{body='binary\0data',stream=true}))
assert(response.headers['x-proxy']=='visited' and response.body:read_all()=='/via?x=1 binary\0data')
assert(client:request('GET',root..'/via').headers['x-proxy']=='visited')
assert(peers[1]==peers[2],'same client did not reuse proxy connection')
assert(client:request('GET',root..'/redirect').body=='/bypass ')
assert(decisions[#decisions]=='/bypass' and proxy_requests==3)
local independent=assert(Client.new({proxy=proxy}))
assert(independent:request('GET',root..'/via').status==200)
assert(peers[#peers]~=peers[1],'different clients shared proxy connections')
bad(client:request('GET',root,{proxy=proxy}))
bad(client:request('GET',root,{headers={['proxy-authorization']='forbidden'}}))
local failing=Client.new({proxy=function() return nil,'route failed' end})
local value,err=failing:request('GET',root);assert(value==nil and err=='route failed')
assert(direct:request('GET',root..'/ready').body=='/ready ')
forward:close();origin:close()
print('HTTP forward proxy, authentication, per-hop routing, streaming and pool isolation passed')
