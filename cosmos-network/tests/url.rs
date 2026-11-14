//! URL 模块注入、实际 HTTP 合并、原生语义及有界序列转换。
use cosmos::Machine;
use cosmos_network::Network;
use std::time::Duration;
fn runtime(multi: bool) -> tokio::runtime::Runtime {
    let mut builder = if multi {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    builder.worker_threads(2).enable_all().build().unwrap()
}
fn execute(runtime: &tokio::runtime::Runtime, machine: &mut Machine, source: &str) {
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(20), machine.exec(source))
            .await
            .unwrap()
            .unwrap();
    });
}
fn quote(value: &str) -> String {
    let mut out = String::from("'");
    for byte in value.bytes() {
        out.push_str(&format!("\\{byte:03}"));
    }
    out.push('\'');
    out
}
#[test]
fn url_example_and_http_redirects_work_on_both_injected_runtimes() {
    for multi in [false, true] {
        let runtime = runtime(multi);
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        execute(
            &runtime,
            &mut machine,
            "assert(cosmos.network==nil and not pcall(require,'cosmos.network.http.url'))",
        );
        machine.add_module(Network).unwrap();
        machine.add_module(Network).unwrap();
        execute(
            &runtime,
            &mut machine,
            "assert(cosmos.encoding==nil and cosmos.text==nil)",
        );
        execute(
            &runtime,
            &mut machine,
            include_str!("../../examples/url.lua"),
        );
    }
}
#[test]
fn parse_join_and_query_decoding_match_the_native_backend() {
    let runtime = runtime(false);
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    for raw in [
        "HTTPS://EXAMPLE.COM:443/a/../b?x=a+b&x=%2B#f",
        "http://0177.0.0.1/",
        "http://[::1]:8080/",
        "https://b\u{fc}cher.example/\u{4e2d}\u{6587}?q=\u{4e2d}",
        "mailto:person@example.com?subject=hello+world",
        "file:///tmp/a%20b",
        "data:text/plain,value",
        "https://u%40ser:p%3Ass@example.com/?a&a=&=b&&a=%FF&b=%ZZ#",
        "https://example.com/?",
        "custom://server/a",
    ] {
        let native = url::Url::parse(raw).unwrap();
        let mut script=format!("local u=assert(cosmos.network.http.url.parse({}));assert(u:string()=={});assert(u:path()=={});",quote(raw),quote(native.as_str()),quote(native.path()));
        let pairs = native.query_pairs().collect::<Vec<_>>();
        script.push_str(&format!(
            "local q=assert(u:query_pairs());assert(#q=={});",
            pairs.len()
        ));
        for (index, (name, value)) in pairs.iter().enumerate() {
            script.push_str(&format!(
                "assert(q[{}].name=={} and q[{}].value=={});",
                index + 1,
                quote(name),
                index + 1,
                quote(value)
            ));
        }
        for reference in [
            "../c/./d",
            "?q=2",
            "#fragment",
            "",
            "//Elsewhere.example/p",
            "https://other.example/a",
            "/%2e%2e/x",
        ] {
            match native.join(reference) {
                Ok(joined) => script.push_str(&format!(
                    "assert(u:join({}):string()=={});",
                    quote(reference),
                    quote(joined.as_str())
                )),
                Err(_) => script.push_str(&format!(
                    "do local v,e=u:join({});assert(v==nil and type(e)=='string') end;",
                    quote(reference)
                )),
            }
        }
        execute(&runtime, &mut machine, &script);
    }
}
#[test]
fn limits_sparse_inputs_lifetimes_and_errors_do_not_mutate_the_base() {
    let runtime = runtime(false);
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Network).unwrap();
    execute(
        &runtime,
        &mut machine,
        r#"
        local Url=cosmos.network.http.url
        local base=assert(Url.parse('https://example.com/base?q=1#f'))
        local function bad(v,e) assert(v==nil and type(e)=='string',tostring(e)) end
        bad(Url.parse(string.rep('x',65537)))
        bad(base:join(string.rep('x',65537)))
        bad(base:with_query(string.rep('x',65537)))
        bad(base:with_path(string.rep('x',65536)))
        bad(base:with_fragment(string.rep(' ',30000)))
        bad(base:with_username('\255'));bad(base:with_password('\255'))
        bad(base:with_host('\255'));bad(base:with_path('\255'))
        bad(base:with_query('\255'));bad(base:with_fragment('\255'))
        bad(base:with_port(0/0));bad(base:with_port(math.huge))
        assert(base:with_port(0):port()==0 and base:with_port(65535):port()==65535)
        local many={};for i=1,1025 do many[i]={name='q',value=tostring(i)} end
        bad(base:with_query_pairs(many))
        many[1025]=nil
        assert(#base:with_query_pairs(many):query_pairs()==1024)
        bad(Url.parse('https://example.com/?'..string.rep('q=x&',1024)..'q=x'):query_pairs())
        local paths={};for i=1,1025 do paths[i]='a' end
        bad(base:with_path_segments(paths))
        bad(Url.parse('https://example.com/'..string.rep('a/',1024)):path_segments())
        bad(base:with_path_segments({[0]='x'}));bad(base:with_path_segments({[1.5]='x'}))
        bad(base:with_query_pairs({{name='q',value='\255'}}))
        bad(base:with_query_pairs({{name='q',value=string.rep('x',65536)}}))
        bad(base:with_query_pairs({{name='q',value=string.rep('%',30000)}}))
        local sparse=setmetatable({[2]={name='q',value='v'}},{__index=function() return {name='q',value='v'} end})
        bad(base:with_query_pairs(sparse))
        bad(base:with_query_pairs({setmetatable({},{__index=function(_,k) return 'ignored-'..k end})}))
        local value={name=('borrowed-name'):rep(1024),value=('borrowed-value'):rep(1024)}
        local edited=assert(base:with_query_pairs({value}))
        value=nil;collectgarbage('collect');collectgarbage('collect')
        assert(edited:query_pairs()[1].name==('borrowed-name'):rep(1024))
        local source=Url.parse('https://example.com/'..string.rep('a',8192))
        local held=assert(source:join('?q=retained'));source=nil
        collectgarbage('collect');assert(held:query()=='q=retained')
        assert(base:string()=='https://example.com/base?q=1#f')
        assert(base:join('/ready'):path()=='/ready')
        package.loaded['cosmos.network.http.url']=nil
        assert(require('cosmos.network.http.url')==Url)
    "#,
    );
}
