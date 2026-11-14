//! builtin FS / 时间不依赖插件，验证两种宿主 Runtime 及系统资源释放。
use cosmos::Machine;
use std::time::Duration;

#[test]
fn builtin_filesystem_and_datetime_work_on_both_runtime_flavors() {
    for multithread in [false, true] {
        let mut builder = if multithread {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = builder.worker_threads(2).enable_all().build().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut machine = Machine::new(runtime.handle().clone()).unwrap();
        runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(10),
                machine.exec_with_args(
                    "=fs.lua",
                    include_str!("../../examples/fs.lua"),
                    &[directory.path().to_str().unwrap().to_owned()],
                ),
            )
            .await
            .unwrap()
            .unwrap();
            tokio::time::timeout(
                Duration::from_secs(10),
                machine.exec(include_str!("../../examples/datetime.lua")),
            )
            .await
            .unwrap()
            .unwrap();
            machine
                .exec("assert(cosmos.network == nil and cosmos.encoding == nil)")
                .await
                .unwrap();
        });
    }
}

#[cfg(unix)]
#[test]
fn filesystem_round_trips_non_utf8_paths_and_distinguishes_symlinks() {
    use std::os::unix::ffi::OsStringExt;
    let directory = tempfile::tempdir().unwrap();
    let filename = std::ffi::OsString::from_vec(vec![b'a', 255]);
    let raw_available = match std::fs::write(directory.path().join(&filename), b"raw bytes") {
        Ok(()) => true,
        Err(error)
            if error.raw_os_error() == Some(libc::EILSEQ)
                || error.kind() == std::io::ErrorKind::InvalidInput =>
        {
            eprintln!("Host filesystem rejects non-UTF-8 filenames; testing byte-preserving path operations instead");
            false
        }
        Err(error) => panic!("cannot create filename fixture: {error}"),
    };
    let unicode = "unicode-\u{4e2d}\u{6587}";
    std::fs::write(directory.path().join(unicode), b"unicode bytes").unwrap();
    std::os::unix::fs::symlink(unicode, directory.path().join("link")).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    runtime.block_on(machine.exec_with_args("=raw paths", r#"
        local root, raw_available = ...
        local fs, path = cosmos.fs, cosmos.fs.path
        local raw = path.join(root, 'a\255')
        assert(path.filename(raw) == 'a\255')
        if raw_available == 'yes' then assert(fs.read_file(raw) == 'raw bytes') end
        local unicode = 'unicode-\228\184\173\230\150\135'
        assert(fs.read_file(path.join(root, unicode)) == 'unicode bytes')
        local link = path.join(root, 'link')
        assert(fs.symlink_metadata(link).kind == 'symlink')
        assert(fs.metadata(link).kind == 'file')
        assert(fs.read_link(link) == unicode and fs.canonicalize(link) == path.join(assert(fs.canonicalize(root)),unicode))
        local dir = assert(fs.read_dir(root)); local found = false
        while true do local item = dir:next(); if not item then break end; if item.name == 'a\255' then found = true end end
        assert(found == (raw_available == 'yes') and dir:close())
    "#, &[directory.path().to_str().unwrap().to_owned(), if raw_available { "yes" } else { "no" }.to_owned()])).unwrap();
}

#[test]
fn machine_drop_releases_file_locks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("locked");
    std::fs::write(&path, []).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    runtime
        .block_on(machine.exec_with_args(
            "=file ownership",
            r#"
        local path = ...
        held = assert(cosmos.fs.open(path, 'r+'))
        assert(held:lock())
        cosmos.thread.spawn(function() cosmos.time.sleep(1e12); held:metadata() end)
        cosmos.time.sleep(0)
    "#,
            &[path.to_str().unwrap().to_owned()],
        ))
        .unwrap();
    let peer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert!(matches!(
        peer.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(machine);
    peer.try_lock().unwrap();
    peer.unlock().unwrap();
}

#[test]
fn filesystem_preserves_submillisecond_mtime_for_change_detection() {
    use std::fs::FileTimes;
    use std::time::UNIX_EPOCH;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mtime");
    let file = std::fs::File::create(&path).unwrap();
    let before = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_100);
    let after = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_200);
    file.set_times(FileTimes::new().set_modified(before))
        .unwrap();
    let first = file
        .metadata()
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut machine = Machine::new(runtime.handle().clone()).unwrap();
    runtime
        .block_on(machine.exec_with_args(
            "=mtime snapshot",
            r#"
        local path, expected = ...
        snapshot = assert(cosmos.fs.metadata(path))
        assert(snapshot.modified_ns == expected)
        assert(cosmos.time.datetime.from_timestamp_ns(expected):timestamp_ns() == expected)
    "#,
            &[
                path.to_str().unwrap().to_owned(),
                first.as_nanos().to_string(),
            ],
        ))
        .unwrap();
    file.set_times(FileTimes::new().set_modified(after))
        .unwrap();
    let second = file
        .metadata()
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    runtime
        .block_on(machine.exec_with_args(
            "=mtime update",
            r#"
        local path, expected, changed = ...
        local current = assert(cosmos.fs.metadata(path))
        assert(current.modified_ns == expected)
        if changed == 'yes' then
            assert(current.modified_ns ~= snapshot.modified_ns)
            assert(current.modified_ms == snapshot.modified_ms)
        end
    "#,
            &[
                path.to_str().unwrap().to_owned(),
                second.as_nanos().to_string(),
                if first != second { "yes" } else { "no" }.to_owned(),
            ],
        ))
        .unwrap();
    if first == second {
        eprintln!("Host filesystem does not preserve 100 ns mtime differences; exact reported timestamps were verified");
    }
}
