-- Network 自带 URL；修改返回新句柄，不改变原值或调用者共享的 URL。
local Url = require('cosmos.network.http.url')
assert(Url == cosmos.network.http.url and require('cosmos.network.http') == cosmos.network.http)
local url = assert(Url.parse('HTTPS://user:pass@Example.COM:443/a/../b/%2F?q=a+b&q=%2B&empty=#part'))
assert(url:string() == 'https://user:pass@example.com/b/%2F?q=a+b&q=%2B&empty=#part')
assert(url:scheme() == 'https' and url:host() == 'example.com')
assert(url:username() == 'user' and url:password() == 'pass')
assert(url:port() == nil and url:port_or_default() == 443)
assert(url:path() == '/b/%2F' and url:query() == 'q=a+b&q=%2B&empty=' and url:fragment() == 'part')
assert(url:can_be_base() and url:origin() == 'https://example.com')
local segments = assert(url:path_segments())
assert(#segments == 2 and segments[1] == 'b' and segments[2] == '%2F')
local pairs = assert(url:query_pairs())
assert(#pairs == 3 and pairs[1].name == 'q' and pairs[1].value == 'a b')
assert(pairs[2].name == 'q' and pairs[2].value == '+' and pairs[3].value == '')
local changed = assert(url:with_query_pairs({{name='q',value='a b'}, {name='q',value='+'}, {name='binary',value='a\0b'}}))
assert(changed:query() == 'q=a+b&q=%2B&binary=a%00b')
assert(changed:query_pairs()[3].value == 'a\0b' and url:query() == 'q=a+b&q=%2B&empty=')
assert(url:with_query('q=a+b&q=%2B'):query() == 'q=a+b&q=%2B')
assert(url:with_query(''):query() == '' and url:with_query(nil):query() == nil)
assert(Url.parse('https://example.com/?q=a%20b&q=d,s'):append_query_pair('q','+'):query() == 'q=a%20b&q=d,s&q=%2B')
assert(url:with_query_pairs({}):query() == '' and url:with_fragment(nil):fragment() == nil)
assert(url:with_fragment(''):fragment() == '')
assert(url:with_path('/one two/../three'):path() == '/three')
local safe = assert(url:with_path_segments({'one/two', '100%', 'a?b#c'}))
assert(safe:path() == '/one%2Ftwo/100%25/a%3Fb%23c')
assert(url:with_path_segments({}):path() == '/')
local clean = assert(assert(url:with_username('')):with_password(nil))
assert(clean:username() == '' and clean:password() == nil and not clean:string():find('@', 1, true))
assert(clean:with_host('other.example'):host() == 'other.example')
assert(clean:with_port(8443):port() == 8443 and clean:with_port(8443):with_port(nil):port() == nil)
assert(clean:with_scheme('http'):port_or_default() == 80)
assert(Url.parse('https://EXAMPLE.com:443/other'):same_origin(clean))
assert(not Url.parse('http://example.com/'):same_origin(clean))
assert(not Url.parse('https://example.com:8443/'):same_origin(clean))
local base = assert(Url.parse('https://example.com/a/b?old=1#old'))
assert(base:join('../c/./d?q=2#new'):string() == 'https://example.com/c/d?q=2#new')
assert(base:join('?new=2'):string() == 'https://example.com/a/b?new=2')
assert(base:join('#new'):string() == 'https://example.com/a/b?old=1#new')
assert(base:join('//Other.example/x'):string() == 'https://other.example/x')
assert(base:join(''):string() == 'https://example.com/a/b?old=1')
assert(base:join('https://elsewhere.example/'):host() == 'elsewhere.example')
local ip = assert(Url.parse('http://[::1]:8080/a'))
assert(ip:host() == '[::1]' and ip:port() == 8080 and ip:origin() == 'http://[::1]:8080')
assert(Url.parse('http://127.1/'):host() == '127.0.0.1')
local opaque = assert(Url.parse('mailto:user@example.com'))
assert(not opaque:can_be_base() and opaque:origin() == nil and not opaque:same_origin(opaque))
assert(opaque:path() == 'user@example.com' and opaque:host() == nil)
assert(Url.parse('file:///tmp/a'):path() == '/tmp/a')
local ws = assert(Url.parse('ws://localhost:9222/endpoint?token=x'))
ws = assert(assert(ws:with_host('[::1]')):with_port(9333))
assert(ws:string() == 'ws://[::1]:9333/endpoint?token=x')

-- Unicode 主机使用 IDNA；路径和查询按各自规则编码，不能先给整个 URL 做百分号编码。
local idn = assert(Url.parse('https://b\195\188cher.example/\228\184\173\230\150\135?q=\228\184\173\230\150\135'))
assert(idn:host() == 'xn--bcher-kva.example' and idn:path() == '/%E4%B8%AD%E6%96%87')
assert(idn:query_pairs()[1].value == '\228\184\173\230\150\135')
local invalid = assert(Url.parse('https://example.com/?q=%FF&bad=%ZZ')):query_pairs()
assert(invalid[1].value == '\239\191\189' and invalid[2].value == '%ZZ')
local function bad(value, err)
    assert(value == nil and type(err) == 'string', tostring(err))
end
bad(Url.parse('relative/path'))
bad(Url.parse('https://[invalid]/'))
bad(Url.parse('\255'))
bad(base:join('\255'))
bad(opaque:join('relative'))
bad(opaque:path_segments())
bad(opaque:with_path_segments({'x'}))
bad(base:with_host(nil))
bad(base:with_host('bad host'))
bad(base:with_scheme('invalid scheme'))
bad(base:with_scheme('mailto'))
bad(base:with_port(-1))
bad(base:with_port(65536))
bad(base:with_port(1.5))
bad(base:with_query_pairs({{name='q'}}))
bad(base:with_query_pairs({[2]={name='q',value='x'}}))
bad(base:with_path_segments({x='a'}))
local cycle = {};cycle[1] = cycle
bad(base:with_path_segments(cycle))
assert(base:string() == 'https://example.com/a/b?old=1#old')
print('URL parsing, components, references, queries and immutable edits passed')

-- 真实 HTTP 重定向复用 URL 语义；fragment 不上网，跨源剥离凭证。
local Server, Client = cosmos.network.http.server, cosmos.network.http.client
local other_count, requests, fragment_sent = 0, 0, false
local other = assert(Server.listen('127.0.0.1', 0, function(req)
    other_count = other_count + 1
    return {body=req:header('authorization') or 'none'}
end))
local server
server = assert(Server.listen('127.0.0.1', 0, function(req)
    requests = requests + 1
    local path = req:target()
    assert(not path:find('#', 1, true), 'HTTP fragment reached the server')
    if path == '/a/start?old=1' then
        return {status=302, headers={location='../b/./middle?x=1#hidden'}}
    elseif path == '/b/middle?x=1' then
        return {status=302, headers={location='?x=2'}}
    elseif path == '/b/middle?x=2' and not fragment_sent then
        fragment_sent = true
        return {status=302, headers={location='#part'}}
    elseif path == '/b/middle?x=2' then
        return {status=302, headers={location='//127.0.0.1:' .. server:port() .. '/b/./final?x=3#drop'}}
    elseif path == '/b/final?x=3' then
        return {body=path .. ' ' .. (req:header('authorization') or 'none')}
    elseif path == '/cross' then
        return {status=302, headers={location='http://127.0.0.1:' .. other:port() .. '/end'}}
    elseif path == '/bad-userinfo' then
        return {status=302, headers={location='http://user:pass@127.0.0.1:' .. other:port() .. '/end'}}
    end
    return {body=path}
end))
local root = assert(Url.parse('http://127.0.0.1:' .. server:port()))
local client = assert(Client.new())
local response = assert(client:request('GET', root:join('/a/start?old=1'):string(), {headers={authorization='Bearer secret'}}))
assert(response.body == '/b/final?x=3 Bearer secret' and requests == 5)
assert(client:request('GET', root:join('/cross'):string(), {headers={authorization='Bearer secret'}}).body == 'none')
assert(client:request('GET', root:string() .. 'a/../echo space?x=a b#hidden').body == '/echo%20space?x=a%20b')
local before = other_count
bad(client:request('GET', root:join('/bad-userinfo'):string()))
assert(other_count == before, 'redirect userinfo dispatched a request')
local before = requests
bad(client:request('GET', root:with_username('user'):string()))
bad(client:request('GET', 'mailto:user@example.com'))
bad(client:request('GET', 'not a URL'))
assert(requests == before, 'invalid URL dispatched a request')
assert(client:request('GET', root:join('/ready'):string()).body == '/ready')
server:close();other:close()
print('HTTP URL normalization, reference redirects, fragments and origins passed')
