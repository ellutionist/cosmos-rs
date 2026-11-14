//! 用受控服务端覆盖认证分支、错误边界和 MySQL 物理包分帧。
#![cfg(feature = "mysql")]

use cosmos::Machine;
use cosmos_database::Database;
use rand::SeedableRng;
use rsa::pkcs8::{EncodePublicKey, LineEnding};
use rsa::{Oaep, RsaPrivateKey};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const FULL: usize = 0x00ff_ffff;
const SCRAMBLE: &[u8; 20] = b"0123456789abcdefghij";
const OK: &[u8] = b"\x00\x00\x00\x02\x00\x00\x00";
const EOF: &[u8] = b"\xfe\x00\x00\x02\x00";

async fn frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0; 4];
    stream.read_exact(&mut header).await.unwrap();
    let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await.unwrap();
    (header[3], bytes)
}

async fn send(stream: &mut TcpStream, mut number: u8, payload: &[u8]) -> u8 {
    let mut offset = 0;
    loop {
        let length = (payload.len() - offset).min(FULL);
        let mut header = (length as u32).to_le_bytes();
        header[3] = number;
        stream.write_all(&header).await.unwrap();
        stream
            .write_all(&payload[offset..offset + length])
            .await
            .unwrap();
        number = number.wrapping_add(1);
        offset += length;
        if length < FULL {
            return number;
        }
    }
}

async fn greeting(stream: &mut TcpStream, plugin: &str) -> Vec<u8> {
    let flags: u32 = 0x000b_a20f;
    let mut payload = b"\x0a8.0-cosmos-test\0".to_vec();
    payload.extend(1u32.to_le_bytes());
    payload.extend(&SCRAMBLE[..8]);
    payload.push(0);
    payload.extend((flags as u16).to_le_bytes());
    payload.push(45);
    payload.extend(2u16.to_le_bytes());
    payload.extend(((flags >> 16) as u16).to_le_bytes());
    payload.push(21);
    payload.extend([0; 10]);
    payload.extend(&SCRAMBLE[8..]);
    payload.push(0);
    payload.extend(plugin.as_bytes());
    payload.push(0);
    send(stream, 0, &payload).await;
    let (number, reply) = frame(stream).await;
    assert_eq!(number, 1);
    let flags = u32::from_le_bytes(reply[..4].try_into().unwrap());
    assert_eq!(
        flags & 0x80,
        0,
        "the client must not advertise LOCAL INFILE"
    );
    assert_eq!(reply[8], 45);
    let user_end = reply[32..].iter().position(|byte| *byte == 0).unwrap() + 32;
    assert_eq!(&reply[32..user_end], b"user");
    let size = reply[user_end + 1] as usize;
    reply[user_end + 2..user_end + 2 + size].to_vec()
}

fn native_token(password: &[u8]) -> Vec<u8> {
    let first = Sha1::digest(password);
    let second = Sha1::digest(first);
    let mut digest = Sha1::new();
    digest.update(SCRAMBLE);
    digest.update(second);
    first
        .iter()
        .zip(digest.finalize())
        .map(|(a, b)| a ^ b)
        .collect()
}

fn run_server_case(server: impl AsyncFnOnce(TcpStream), script: impl FnOnce(u16) -> String) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let source = script(listener.local_addr().unwrap().port());
        let server = async {
            let (stream, _) = listener.accept().await.unwrap();
            server(stream).await;
        };
        let client = async {
            let mut machine = Machine::new(tokio::runtime::Handle::current()).unwrap();
            machine.add_module(Database).unwrap();
            machine.exec(&source).await.unwrap();
        };
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            tokio::join!(server, client);
        })
        .await
        .unwrap();
    });
}

#[test]
fn native_caching_sha2_sha256_and_ed25519_authentication() {
    for mode in [
        "native",
        "caching_sha2",
        "caching_sha2_switch",
        "sha256",
        "sha256_switch",
        "ed25519",
    ] {
        let password = if mode == "ed25519" {
            "password"
        } else {
            "long-password-crosses-twenty-byte-scramble-boundary"
        };
        run_server_case(
            async move |mut stream| {
                let auth_mode = mode.strip_suffix("_switch").unwrap_or(mode);
                let plugin = match auth_mode {
                    "caching_sha2" => "caching_sha2_password",
                    "sha256" => "sha256_password",
                    _ => "mysql_native_password",
                };
                let initial_plugin = if mode.ends_with("_switch") {
                    "mysql_native_password"
                } else {
                    plugin
                };
                let mut token = greeting(&mut stream, initial_plugin).await;
                let mut number = 2;
                if mode.ends_with("_switch") {
                    assert_eq!(token, native_token(password.as_bytes()));
                    let switch =
                        [b"\xfe".as_slice(), plugin.as_bytes(), &[0], SCRAMBLE, &[0]].concat();
                    number = send(&mut stream, number, &switch).await;
                    let (received, response) = frame(&mut stream).await;
                    assert_eq!(received, number);
                    number += 1;
                    token = response;
                }
                if mode == "native" || mode == "ed25519" {
                    assert_eq!(token, native_token(password.as_bytes()));
                } else if auth_mode == "caching_sha2" {
                    let first = Sha256::digest(password.as_bytes());
                    let second = Sha256::digest(first);
                    let third = Sha256::digest([second.as_slice(), SCRAMBLE].concat());
                    let expected: Vec<_> = first.iter().zip(third).map(|(a, b)| a ^ b).collect();
                    assert_eq!(token, expected);
                    number = send(&mut stream, number, &[1, 4]).await;
                    assert_eq!(frame(&mut stream).await, (number, vec![2]));
                    number += 1;
                } else {
                    assert_eq!(token, [1]);
                }
                if auth_mode == "caching_sha2" || auth_mode == "sha256" {
                    let mut random = rand::rngs::StdRng::seed_from_u64(7);
                    let private = RsaPrivateKey::new(&mut random, 1024).unwrap();
                    let pem = private
                        .to_public_key()
                        .to_public_key_pem(LineEnding::LF)
                        .unwrap();
                    let payload = [b"\x01".as_slice(), pem.as_bytes()].concat();
                    number = send(&mut stream, number, &payload).await;
                    let (received, encrypted) = frame(&mut stream).await;
                    assert_eq!(received, number);
                    number += 1;
                    let scrambled = private.decrypt(Oaep::new::<Sha1>(), &encrypted).unwrap();
                    let clear: Vec<_> = scrambled
                        .iter()
                        .enumerate()
                        .map(|(i, byte)| byte ^ SCRAMBLE[i % SCRAMBLE.len()])
                        .collect();
                    assert_eq!(clear, [password.as_bytes(), &[0]].concat());
                }
                if mode == "ed25519" {
                    let switch = [b"\xfeclient_ed25519\0".as_slice(), &[0x42; 32]].concat();
                    number = send(&mut stream, number, &switch).await;
                    let (received, signature) = frame(&mut stream).await;
                    assert_eq!(received, number);
                    number += 1;
                    assert_eq!(signature.len(), 64);
                    assert_eq!(
                        &signature[..8],
                        &[0x6f, 0x25, 0xec, 0xb9, 0xd3, 0x6d, 0x41, 0x7d]
                    );
                }
                send(&mut stream, number, OK).await;
                assert_eq!(frame(&mut stream).await, (0, vec![1]));
            },
            |port| {
                format!(
                    r#"
                local db = require('cosmos.database.mysql').new()
                assert(db:connect({{host='127.0.0.1', port={port}, user='user', password='{password}'}}))
                assert(db:server_ver() == '8.0-cosmos-test')
                assert(db:close())
            "#
                )
            },
        );
    }
}

fn column() -> Vec<u8> {
    let mut bytes = b"\x03def\x00\x00\x00\x05value\x00\x0c".to_vec();
    bytes.extend(63u16.to_le_bytes());
    bytes.extend((32u32 * 1024 * 1024).to_le_bytes());
    bytes.push(0xfb);
    bytes.extend([0; 5]);
    bytes
}

#[test]
fn malformed_sequences_rows_and_oversized_packets_close_the_connection() {
    for mode in ["sequence", "row", "oversized", "local_infile"] {
        run_server_case(
            async move |mut stream| {
                greeting(&mut stream, "mysql_native_password").await;
                send(&mut stream, 2, OK).await;
                let (number, query) = frame(&mut stream).await;
                assert_eq!(number, 0);
                assert_eq!(query[0], 3);
                match mode {
                    "sequence" => {
                        send(&mut stream, 2, OK).await;
                    }
                    "oversized" => {
                        stream.write_all(&[1, 0, 0x20, 1]).await.unwrap();
                    }
                    "local_infile" => {
                        send(&mut stream, 1, b"\xfbfile").await;
                    }
                    _ => {
                        send(&mut stream, 1, &[1]).await;
                        send(&mut stream, 2, &column()).await;
                        send(&mut stream, 3, EOF).await;
                        send(&mut stream, 4, b"\x05a").await;
                    }
                }
                let mut end = [0];
                match stream.read(&mut end).await {
                    Ok(0) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) => {}
                    other => panic!("connection must close after a protocol error: {other:?}"),
                }
            },
            |port| {
                format!(
                    r#"
                local db = require('cosmos.database.mysql').new()
                assert(db:connect({{host='127.0.0.1', port={port}, user='user'}}))
                local value, err = db:query('SELECT 1')
                assert(value == nil and type(err) == 'string')
                assert(db:query('SELECT 2') == nil)
            "#
                )
            },
        );
    }
}

#[test]
fn full_size_requests_and_binary_rows_span_physical_packets() {
    run_server_case(
        async |mut stream| {
            greeting(&mut stream, "mysql_native_password").await;
            send(&mut stream, 2, OK).await;
            let (number, query) = frame(&mut stream).await;
            assert_eq!(number, 0);
            assert_eq!(query.len(), FULL);
            assert_eq!(query[0], 3);
            assert_eq!(frame(&mut stream).await, (1, vec![]));
            send(&mut stream, 2, &[1]).await;
            send(&mut stream, 3, &column()).await;
            send(&mut stream, 4, EOF).await;
            let size = FULL + 17;
            let mut row = vec![0xfe];
            row.extend((size as u64).to_le_bytes());
            row.extend(vec![0xff; size]);
            let next = send(&mut stream, 5, &row).await;
            send(&mut stream, next, EOF).await;
            assert_eq!(frame(&mut stream).await, (0, vec![1]));
        },
        |port| {
            format!(
                r#"
            local db = require('cosmos.database.mysql').new()
            assert(db:connect({{host='127.0.0.1', port={port}, user='user', max_packet_size=32*1024*1024}}))
            local rows = assert(db:query(string.rep('x', {FULL}-1)))
            assert(#rows == 1 and #rows[1].value == {FULL}+17)
            assert(rows[1].value == string.rep(string.char(255), {FULL}+17))
            assert(db:close())
        "#
            )
        },
    );
}
