//! Redis URL、RESP2 错误边界、认证、TLS/Unix 与显式实机回归。
#![cfg(feature = "redis")]

use cosmos::Machine;
use cosmos_database::Database;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn line(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        bytes.push(stream.read_u8().await.unwrap());
        assert!(bytes.len() < 128);
        if bytes.ends_with(b"\r\n") {
            bytes.truncate(bytes.len() - 2);
            return bytes;
        }
    }
}
async fn request(stream: &mut TcpStream) -> Vec<Vec<u8>> {
    let header = line(stream).await;
    assert_eq!(header[0], b'*');
    let count = std::str::from_utf8(&header[1..])
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let mut values = Vec::new();
    for _ in 0..count {
        let header = line(stream).await;
        assert_eq!(header[0], b'$');
        let length = std::str::from_utf8(&header[1..])
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).await.unwrap();
        let mut ending = [0; 2];
        stream.read_exact(&mut ending).await.unwrap();
        assert_eq!(ending, *b"\r\n");
        values.push(bytes);
    }
    values
}
async fn closed(stream: &mut TcpStream) {
    match stream.read(&mut [0; 1]).await {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {}
        other => panic!("connection should be closed: {other:?}"),
    }
}
fn run_case(server: impl AsyncFnOnce(TcpStream), script: impl FnOnce(u16) -> String) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let source = script(listener.local_addr().unwrap().port());
        let server = async {
            server(listener.accept().await.unwrap().0).await;
        };
        let client = async {
            let mut machine = Machine::new(tokio::runtime::Handle::current()).unwrap();
            machine.add_module(Database).unwrap();
            machine.exec(&source).await.unwrap();
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(server, client);
        })
        .await
        .unwrap();
    });
}

#[test]
fn redis_url_and_module_identity() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Database).unwrap();
    runtime.block_on(machine.exec(r#"
        local redis = require('cosmos.database.redis')
        assert(redis == cosmos.database.redis and redis.null == cosmos.null)
        local opts = assert(redis.parse_url('rediss://u%40ser:p%3Ass@[::1]:6380/2?ssl_verify=false'))
        assert(opts.host == '::1' and opts.port == 6380 and opts.db == 2)
        assert(opts.username == 'u@ser' and opts.password == 'p:ss' and opts.ssl and not opts.ssl_verify)
        assert(redis.parse_url('redis://localhost').port == 6379)
        assert(redis.parse_url('redis://localhost').password == nil)
        for _, url in ipairs({'http://localhost/0', 'redis:///0', 'redis://localhost/-1',
            'redis://localhost/1/2', 'redis://user@localhost/0', 'redis://:p%GG@localhost/0',
            'redis://localhost/0?unknown=1', 'redis://localhost/0?ssl_verify=maybe'}) do
            assert(redis.parse_url(url) == nil)
        end
        assert(redis.client({pool_size=0}) == nil)
        assert(redis.client({idle_timeout=0/0}) == nil)
        assert(redis.client({backlog=1}) == nil)
        local conn = redis.new()
        assert(conn:set_timeouts(1,2,3))
        assert(conn:set_timeout(-1) == nil)
        assert(conn:close())
        redis_before, redis_get_before = redis, redis.get
    "#)).unwrap();
    machine.add_module(Database).unwrap();
    runtime.block_on(machine.exec("assert(cosmos.database.redis == redis_before and redis_before.get == redis_get_before)")).unwrap();
}

#[test]
fn redis_auth_select_binary_and_nested_replies() {
    run_case(
        async |mut stream| {
            assert_eq!(
                request(&mut stream).await,
                [b"auth".to_vec(), b"user".to_vec(), b"p\0\xff".to_vec()]
            );
            stream.write_all(b"+OK\r\n").await.unwrap();
            assert_eq!(
                request(&mut stream).await,
                [b"select".to_vec(), b"2".to_vec()]
            );
            stream.write_all(b"+OK\r\n").await.unwrap();
            assert_eq!(
                request(&mut stream).await,
                [b"echo".to_vec(), b"a\0\xffb".to_vec()]
            );
            for bytes in b"$4\r\na\0\xffb\r\n".chunks(2) {
                stream.write_all(bytes).await.unwrap();
                tokio::task::yield_now().await;
            }
            assert_eq!(request(&mut stream).await[0], b"nested");
            stream.write_all(b"*4\r\n$-1\r\n$0\r\n\r\n:-9223372036854775808\r\n*2\r\n-ERR nested\r\n:+0001\r\n").await.unwrap();
            assert_eq!(request(&mut stream).await[0], b"ping");
            stream
                .write_all(b"-ERR command rejected\r\n")
                .await
                .unwrap();
            assert_eq!(request(&mut stream).await[0], b"ping");
            stream.write_all(b"+PONG\r\n").await.unwrap();
            closed(&mut stream).await;
        },
        |port| {
            format!(
                r#"
        local db = require('cosmos.database.redis').new()
        assert(db:connect({{host='127.0.0.1',port={port},username='user',password='p\0\255',db=2}}))
        assert(db:echo('a\0\255b') == 'a\0\255b')
        local value = assert(db:nested())
        assert(#value == 4 and value[1] == cosmos.null and value[2] == '')
        assert(value[3] == '-9223372036854775808' and value[4][1][1] == false and value[4][2] == 1)
        local failed, err = db:ping()
        assert(failed == false and err == 'ERR command rejected')
        assert(db:ping() == 'PONG')
        assert(db:close())
    "#
            )
        },
    );
}

#[test]
fn invalid_resp_and_limits_discard_connections() {
    for payload in [
        b"$-2\r\n".as_slice(),
        b"$3\r\nabcXX",
        b"+OK\n",
        b"_\r\n",
        b":9223372036854775808\r\n",
        b"$129\r\n",
        b"*5\r\n",
        b"*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n+OK\r\n",
    ] {
        run_case(
            async move |mut stream| {
                assert_eq!(request(&mut stream).await[0], b"ping");
                stream.write_all(payload).await.unwrap();
                closed(&mut stream).await;
            },
            |port| {
                format!(
                    r#"
            local db = require('cosmos.database.redis').new()
            assert(db:connect({{host='127.0.0.1',port={port},max_reply_size=128,max_array_len=4,max_depth=4}}))
            local value, err = db:ping()
            assert(value == nil and type(err) == 'string')
            assert(db:ping() == nil)
        "#
                )
            },
        );
    }
}

#[test]
fn partial_pipeline_and_pubsub_timeouts_cannot_return_to_pool() {
    for subscribe in [false, true] {
        run_case(
            async move |mut stream| {
                if subscribe {
                    assert_eq!(request(&mut stream).await[0], b"subscribe");
                    stream
                        .write_all(b"*3\r\n$9\r\nsubscribe\r\n$1\r\nx\r\n:1\r\n")
                        .await
                        .unwrap();
                } else {
                    assert_eq!(request(&mut stream).await[0], b"ping");
                    assert_eq!(request(&mut stream).await[0], b"ping");
                    stream.write_all(b"+PONG\r\n").await.unwrap();
                }
                closed(&mut stream).await;
            },
            |port| {
                format!(
                    r#"
            local client = assert(require('cosmos.database.redis').client({{host='127.0.0.1',port={port},timeout=30}}))
            local db = assert(client:connect())
            local value, err
            if {subscribe} then
                assert(db:subscribe('x'))
                value, err = db:read_reply()
            else
                assert(db:init_pipeline())
                db:ping(); db:ping()
                value, err = db:commit_pipeline()
            end
            assert(value == nil and err:find('timeout',1,true))
            assert(client:release(db) == nil)
            client:close()
        "#
                )
            },
        );
    }
}

#[test]
fn redis_tls_and_unix_use_the_shared_socket() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Database).unwrap();
    let source = r#"
        local Socket = cosmos.network.socket
        local Tls = cosmos.network.tcp.tls.context
        local server = Tls.new_server()
        server:use_cert_chain_file('examples/certs/cert.pem')
        server:use_private_key_file('examples/certs/key.pem')
        local client = Tls.new()
        client:add_ca_file('examples/certs/ca.pem')
        local function check(listener, options)
            local task = cosmos.thread.spawn(function()
                local peer = assert(listener:accept())
                peer:settimeout(2000)
                assert(peer:starttls_server(server))
                assert(peer:receive(14) == '*1\r\n$4\r\nping\r\n')
                assert(peer:send('+PONG\r\n'))
                peer:abort()
            end)
            local redis = cosmos.database.redis.new()
            options.ssl, options.server_name, options.ctx = true, 'localhost', client
            assert(redis:connect(options))
            assert(redis:ping() == 'PONG')
            redis:close()
            local _, err = task:join(); assert(err == nil, err)
            listener:close()
        end
        local tcp = assert(Socket.listen('127.0.0.1',0))
        check(tcp, {host='127.0.0.1',port=tcp:port()})
        if TEST_UNIX then
            local path = os.tmpname(); os.remove(path)
            local unix = assert(Socket.listen_unix(path))
            check(unix, {path=path})
            os.remove(path)
        end
    "#
    .replace(
        "examples/certs/",
        &format!("{}/../examples/certs/", env!("CARGO_MANIFEST_DIR")),
    )
    .replace("TEST_UNIX", if cfg!(unix) { "true" } else { "false" });
    runtime.block_on(machine.exec(&source)).unwrap();
}

#[test]
fn pipeline_reply_size_limit_is_shared_between_results() {
    run_case(
        async |mut stream| {
            assert_eq!(request(&mut stream).await[0], b"ping");
            assert_eq!(request(&mut stream).await[0], b"ping");
            stream.write_all(b"+PONG\r\n+PONG\r\n").await.unwrap();
            closed(&mut stream).await;
        },
        |port| {
            format!(
                r#"
        local client = assert(cosmos.database.redis.client({{host='127.0.0.1',port={port},max_reply_size=13}}))
        local db = assert(client:connect())
        assert(db:init_pipeline())
        db:ping(); db:ping()
        local value, err = db:commit_pipeline()
        assert(value == nil and err:find('limit',1,true))
        assert(client:release(db) == nil)
        client:close()
    "#
            )
        },
    );
}

#[test]
#[ignore = "requires COSMOS_REDIS_URL; creates and cleans up isolated test keys and temporary ACL users"]
fn redis_example_against_explicit_server() {
    let url = std::env::var("COSMOS_REDIS_URL").expect("set COSMOS_REDIS_URL");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Database).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            machine.exec_with_args(
                "=redis-example",
                include_str!("../../examples/redis.lua"),
                &[url],
            ),
        )
        .await
        .unwrap()
        .unwrap();
    });
}
