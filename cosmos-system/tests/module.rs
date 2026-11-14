use cosmos::Machine;
use cosmos_system::System;
#[test]
fn system_is_optional_and_waiters_stay_on_both_injected_runtimes() {
    for multi in [false, true] {
        let mut builder = if multi {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = builder.worker_threads(2).enable_all().build().unwrap();
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        runtime
            .block_on(machine.exec("assert(cosmos.system==nil)"))
            .unwrap();
        machine.add_module(System).unwrap();
        machine.add_module(System).unwrap();
        runtime
            .block_on(machine.exec(include_str!("../../examples/system.lua")))
            .unwrap();
        runtime.block_on(machine.exec("cosmos.thread.spawn(function() cosmos.system.signal.subscribe('interrupt'):recv() end);cosmos.time.sleep(1)")).unwrap();
        drop(machine);
    }
}
#[cfg(unix)]
#[test]
fn signal_child_entry() {
    if std::env::var_os("COSMOS_SIGNAL_TEST_CHILD").is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    machine.add_module(System).unwrap();
    runtime.block_on(machine.exec("local s=assert(cosmos.system.signal.subscribe('user1'));io.write('signal ready\\n');io.flush();assert(s:recv(3000)=='user1');s:close();print('signal received')")).unwrap();
}
#[cfg(unix)]
#[test]
fn receives_a_real_signal_in_an_owned_child_process() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "signal_child_entry", "--nocapture"])
        .env("COSMOS_SIGNAL_TEST_CHILD", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "signal child exited before readiness"
        );
        if line.contains("signal ready") {
            break;
        }
    }
    assert!(Command::new("kill")
        .args(["-USR1", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
    let mut tail = String::new();
    std::io::Read::read_to_string(&mut output, &mut tail).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(tail.contains("signal received"));
}
