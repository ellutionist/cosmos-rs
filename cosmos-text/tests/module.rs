//! 插件独立注入；实际示例、原生后端互操作、资源与取消边界。
use cosmos::Machine;
use cosmos_text::Text;
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
        tokio::time::timeout(Duration::from_secs(30), machine.exec(source))
            .await
            .unwrap()
            .unwrap();
    });
}
fn quote(text: &str) -> String {
    let mut out = String::from("'");
    for byte in text.bytes() {
        out.push_str(&format!("\\{byte:03}"));
    }
    out.push('\'');
    out
}
#[test]
fn text_works_without_other_plugins_on_both_injected_runtimes() {
    for multi in [false, true] {
        let runtime = runtime(multi);
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        execute(
            &runtime,
            &mut machine,
            "assert(cosmos.text==nil and not pcall(require,'cosmos.text'))",
        );
        machine.add_module(Text).unwrap();
        machine.add_module(Text).unwrap();
        execute(
            &runtime,
            &mut machine,
            "assert(cosmos.network==nil and cosmos.encoding==nil and cosmos.image==nil)",
        );
        execute(
            &runtime,
            &mut machine,
            include_str!("../../examples/text.lua"),
        );
    }
}
#[test]
fn opencc_and_template_expansion_match_native_dependencies() {
    use ferrous_opencc::{config::BuiltinConfig, OpenCC};
    let runtime = runtime(false);
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Text).unwrap();
    for (name, config) in [("s2t", BuiltinConfig::S2t), ("t2s", BuiltinConfig::T2s)] {
        let converter = OpenCC::from_config(config).unwrap();
        for text in [
            "\u{5934}\u{53d1}\u{7406}\u{53d1}\u{53d1}\u{5c55}\u{540e}\u{53f0}",
            "\u{4e7e}\u{676f}\u{4e7e}\u{71e5}\u{5e79}\u{6d3b}\u{5f8c}\u{81fa}",
            "\u{96fb}\u{8166}\u{8edf}\u{4ef6} alpha\0\u{91cd}\u{8907}",
            "",
        ] {
            execute(
                &runtime,
                &mut machine,
                &format!(
                    "assert(cosmos.text.chinese.convert({},'{}')=={})",
                    quote(text),
                    name,
                    quote(&converter.convert(text))
                ),
            );
        }
    }
    let pattern = regex::Regex::new("(?P<word>[a-z]+)([0-9]+)?").unwrap();
    let text = "alpha12 beta";
    for template in [
        "${word}:$2:$$",
        "$word_foo $0 $12",
        "${missing} $9999999999999999999999999",
        "${} ${word} ${ $",
        "$$$1",
        "$2word ${2}word",
        "\u{4e2d}${word}\0",
    ] {
        let expected = pattern.replace_all(text, template);
        execute(
            &runtime,
            &mut machine,
            &format!(
                "local r=assert(cosmos.text.regex.new({}));assert(r:replace({}, {})=={})",
                quote(pattern.as_str()),
                quote(text),
                quote(template),
                quote(&expected)
            ),
        );
    }
}
#[test]
fn limits_fail_without_partial_results_and_operations_recover() {
    let runtime = runtime(false);
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Text).unwrap();
    execute(
        &runtime,
        &mut machine,
        r#"
        local r,u,c=cosmos.text.regex,cosmos.text.unicode,cosmos.text.chinese
        local function bad(value,err) assert(value==nil and type(err)=='string',tostring(err)) end
        bad(r.new(string.rep('a',65537)))
        bad(r.new(string.rep('()',256)))
        bad(r.new('a{1000000000}'))
        bad(r.new(string.rep('(',129)..'a'..string.rep(')',129)))
        local p=assert(r.new('.'))
        bad(p:find_all(string.rep('a',10001)))
        assert(p:find_all(string.rep('a',10001),3):len()==3)
        bad(p:replace(string.rep('a',10001),'b'))
        local called=false
        bad(p:replace(string.rep('a',10001),function() called=true;return 'x' end))
        assert(not called,'callback ran before result budget was checked')
        bad(assert(r.new(',')):split(string.rep(',',10000)))
        assert(#assert(r.new(',')):split('a,b,c',2)==2)
        local huge=string.rep('x',8*1024*1024+1)
        bad(u.len(huge));bad(p:is_match(huge));bad(r.escape(huge))
        bad(c.convert(string.rep('x',1024*1024+1),'t2s'))
        bad(p:replace('x',string.rep('y',65537)))
        local p=assert(r.new('(a+)'))
        bad(p:replace(string.rep('a',1024*1024),string.rep('$1',17)))
        local calls=0
        bad(assert(r.new('x')):replace('xxx',function() calls=calls+1;return string.rep('a',8*1024*1024) end))
        assert(calls==3)
        local captures=assert(r.new(string.rep('(a?)',127)))
        bad(captures:find_all(string.rep('a',1024*1024)))
        assert(u.normalize('ready','nfc')=='ready' and p:is_match('aaa'))
        assert(c.convert('ready','s2t')=='ready')
    "#,
    );
}
#[test]
fn cancellation_and_machine_drop_release_lua_and_leave_workers_responsive() {
    let runtime = runtime(false);
    for _ in 0..2 {
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Text).unwrap();
        execute(
            &runtime,
            &mut machine,
            r#"
            local u,r=cosmos.text.unicode,cosmos.text.regex
            local p=assert(r.new('x'))
            local weak=setmetatable({},{__mode='v'})
            local function start()
                local owner={value=string.rep('a',4*1024*1024)};weak[1]=owner
                return cosmos.thread.spawn(function() u.normalize(owner.value,'nfkc') end)
            end
            local task=start();cosmos.time.sleep(1);task:kill()
            cosmos.time.sleep(1);collectgarbage('collect');collectgarbage('collect')
            assert(weak[1]==nil,'cancelled text job retained Lua references')
            local ticks,running=0,true
            local ticker=cosmos.thread.spawn(function()
                while running do cosmos.time.sleep(1);if running then ticks=ticks+1 end end
            end)
            assert(#u.normalize(string.rep('a',2*1024*1024),'nfkc')==2*1024*1024)
            running=false;assert(ticker:join()==nil and ticks>0,'text worker blocked the host thread')
            local entered=false
            local callback=cosmos.thread.spawn(function()
                p:replace('xx',function() entered=true;cosmos.time.sleep(60000);error('cancelled callback resumed') end)
            end)
            while not entered do cosmos.time.sleep(1) end
            callback:kill()
            assert(p:replace('x','ready')=='ready')
            cosmos.thread.spawn(function() u.normalize(string.rep('a',4*1024*1024),'nfkd') end)
            cosmos.time.sleep(1)
        "#,
        );
        drop(machine);
    }
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Text).unwrap();
    execute(
        &runtime,
        &mut machine,
        "assert(cosmos.text.unicode.upper('ready')=='READY')",
    );
}
#[test]
fn regex_flags_offsets_and_migration_patterns_keep_native_semantics() {
    let runtime = runtime(false);
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Text).unwrap();
    // 原后端的通用正则操作，直接比较 Rust 引擎的捕获与范围；不提供业务专用函数。
    let cases = [
        (
            r"(?is)<think\b[^>]*>.*?</think\s*>",
            "before <THINK x='1'>line\nmore</think > after",
        ),
        (
            r"(?i)(?:https?://|www\.)?(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z]{2,24}\b(?::\d{1,5})?",
            "visit HTTPS://example.com:8080 or www.example.org",
        ),
        (
            r"(?:^|[^A-Z0-9])(FC2[-_ ]?(?:PPV[-_ ]?)?\d{5,9}|[0-9]{0,6}[A-Z]{2,12}[-_ ]\d{2,7})",
            "[AB-123] FC2-PPV-123456 end",
        ),
        (
            r"(?P<word>\w+)(?:-(\d+))?",
            "\u{4e2d}\u{6587}-12 alpha beta-34",
        ),
        (r"a*|b", "ab"),
        (r"", "\u{e9}x"),
    ];
    for (pattern, text) in cases {
        let native = regex::Regex::new(pattern).unwrap();
        let mut assertions = String::new();
        let mut count = 0;
        for (i, captures) in native.captures_iter(text).enumerate() {
            count += 1;
            for (group, value) in captures.iter().enumerate() {
                if let Some(value) = value {
                    assertions.push_str(&format!("do local m=items:get({});assert(m:group({})=={});local a,b=m:range({});assert(a=={} and b=={}) end\n",i+1,group,quote(value.as_str()),group,value.start()+1,value.end()));
                } else {
                    assertions.push_str(&format!(
                        "assert(items:get({}):group({})==nil)\n",
                        i + 1,
                        group
                    ));
                }
            }
        }
        execute(&runtime,&mut machine,&format!("local p=assert(cosmos.text.regex.new({}));local items=assert(p:find_all({}));assert(items:len()=={});{}",quote(pattern),quote(text),count,assertions));
    }
    execute(
        &runtime,
        &mut machine,
        r#"
        local r=cosmos.text.regex
        assert(r.new('^b$',{multi_line=true}):is_match('a\nb\nc'))
        assert(not r.new('^b$'):is_match('a\nb\nc'))
        assert(r.new('a.b',{dot_matches_new_line=true}):is_match('a\nb'))
        assert(r.new('a b # ignored',{ignore_whitespace=true}):is_match('ab'))
        assert(r.new('a+',{swap_greed=true}):find('aaa'):text()=='a')
        assert(r.new('^a'):find('ba',2)==nil)
        assert(r.new('\\bword'):find('xword',2)==nil)
        assert(r.new('a$'):find('ba',2):text()=='a')
        assert(r.new(''):find('',1):range()==1)
        assert(r.new('x'):replace('xx',function() end)=='xx')
        assert(r.new('x'):replace('xx',function() return nil end)=='xx')
        local list=r.new(''):split('ab')
        assert(#list==4 and list[1]=='' and list[2]=='a' and list[3]=='b' and list[4]=='')
        assert(r.new('x'):split('xx',1)[1]=='xx')
    "#,
    );
}
