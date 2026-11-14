//! 两种 Runtime 下的真实 Lua/FS 用例、PHC 互操作以及计算期间 VM 销毁。
use argon2::{password_hash::SaltString, Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use cosmos::Machine;
use cosmos_encryption::Encryption;
use std::time::Duration;

#[test]
fn crypto_works_without_encoding_and_phc_matches_the_backend() {
    for multithread in [false, true] {
        let mut builder = if multithread {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = builder.worker_threads(2).enable_all().build().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Encryption).unwrap();
        machine.add_module(Encryption).unwrap();
        let salt = SaltString::encode_b64(b"backend-test-salt").unwrap();
        let native = Argon2::default()
            .hash_password(b"interop-password", &salt)
            .unwrap()
            .to_string();
        let output = scratch.path().join("phc");
        let source=format!("assert(cosmos.encoding == nil)\nlocal function run(...)\n{}\nend\nrun({:?})\nassert(cosmos.encryption.password.verify('interop-password',{:?}))\nassert(cosmos.fs.write_file({:?},assert(cosmos.encryption.password.hash('interop-password'))))",
            include_str!("../../examples/crypto.lua"),scratch.path().to_str().unwrap(),native,output.to_str().unwrap());
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(40), machine.exec(&source))
                .await
                .unwrap()
                .unwrap();
        });
        let generated = std::fs::read_to_string(&output).unwrap();
        Argon2::default()
            .verify_password(b"interop-password", &PasswordHash::new(&generated).unwrap())
            .unwrap();
        std::fs::remove_file(output).unwrap();
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }
}

#[test]
fn dropping_a_machine_during_password_work_does_not_keep_lua_alive() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for _ in 0..2 {
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        machine.add_module(Encryption).unwrap();
        runtime.block_on(machine.exec(r#"
            local weak=setmetatable({},{__mode='v'})
            local function start()
                local retained={password='pending'}; weak[1]=retained
                return cosmos.thread.spawn(function()
                    cosmos.encryption.password.hash(retained.password,{memory_kib=65536,iterations=3})
                end)
            end
            local task=start()
            cosmos.time.sleep(2)
            task:kill()
            cosmos.time.sleep(1)
            collectgarbage('collect'); collectgarbage('collect')
            assert(weak[1] == nil, 'cancelled worker retained Lua state')
            cosmos.thread.spawn(function()
                cosmos.encryption.password.hash('pending',{memory_kib=65536,iterations=3})
            end)
            cosmos.time.sleep(2)
        "#)).unwrap();
        drop(machine);
    }
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(Encryption).unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(30),machine.exec(r#"
            local encoded=assert(cosmos.encryption.password.hash('recovered',{memory_kib=32,iterations=1}))
            assert(cosmos.encryption.password.verify('recovered',encoded))
        "#)).await.unwrap().unwrap();
    });
}
