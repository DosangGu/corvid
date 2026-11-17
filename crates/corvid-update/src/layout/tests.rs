use super::*;

fn write_file(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn derives_paths_from_the_layout_and_executable_platform() {
    let cases = [
        ("linux", "flat", "flat", "flat", "corvid", "corvid-desktop"),
        ("macos", "flat", "flat", "flat", "corvid", "corvid-desktop"),
        (
            "windows",
            "flat",
            "flat",
            "flat",
            "corvid.exe",
            "corvid-desktop.exe",
        ),
        (
            "macos",
            "version/Corvid.app/Contents/MacOS",
            "version/Corvid.app/Contents/Resources",
            "version",
            "corvid",
            "corvid-desktop",
        ),
    ];
    let root = Path::new("Corvid 설치");
    for (os, binaries, resources, package, daemon, desktop) in cases {
        let executable = root.join(binaries).join(desktop);
        let layout = for_platform(&executable, os).unwrap();
        assert_eq!(layout.package_directory(), root.join(package));
        assert_eq!(layout.desktop(), executable);
        assert_eq!(layout.local_daemon(), root.join(binaries).join(daemon));
        assert_eq!(
            layout.package_metadata(),
            root.join(resources).join("package.json")
        );
        for target in [
            "aarch64-unknown-linux-musl",
            "x86_64-pc-windows-msvc",
            "aarch64-apple-darwin",
        ] {
            assert_eq!(
                layout.remote_runtime(target),
                root.join(resources).join("daemon").join(target)
            );
        }
    }
}

#[test]
fn selects_packaged_daemon_without_requiring_it_in_source_builds() {
    let temp = tempfile::tempdir().unwrap();
    for (os, binary_dir, metadata, daemon, desktop) in [
        (
            "macos",
            "Corvid.app/Contents/MacOS",
            "Corvid.app/Contents/Resources/package.json",
            "corvid",
            "corvid-desktop",
        ),
        (
            "linux",
            "linux",
            "linux/package.json",
            "corvid",
            "corvid-desktop",
        ),
        (
            "windows",
            "windows",
            "windows/package.json",
            "corvid.exe",
            "corvid-desktop.exe",
        ),
    ] {
        let binaries = temp.path().join(binary_dir);
        let executable = binaries.join(desktop);
        let runtime = binaries.join(daemon);
        write_file(&executable, "");
        assert_eq!(
            local_daemon_for_platform(&executable, os).unwrap(),
            executable
        );

        write_file(&temp.path().join(metadata), "{}");
        let error = local_daemon_for_platform(&executable, os).unwrap_err();
        assert_eq!(error.to_string(), "the packaged Corvid runtime is missing");
        write_file(&runtime, "");
        assert_eq!(local_daemon_for_platform(&executable, os).unwrap(), runtime);
        assert_eq!(local_daemon_for_platform(&runtime, os).unwrap(), runtime);
    }
}

#[test]
fn deck_selects_nested_helper_then_packaged_plugin_then_source_binary() {
    let temp = tempfile::tempdir().unwrap();
    let contents = temp.path().join("Corvid.app/Contents");
    let desktop = contents.join("MacOS/corvid-desktop");
    let development = contents.join("MacOS/corvid-agent-deck");
    let packaged = contents.join("MacOS/plugins/agent-deck/corvid-agent-deck");
    let helper = contents.join("Helpers/Corvid Agent Deck.app/Contents/MacOS/corvid-agent-deck");
    assert!(agent_deck_for_platform(&desktop, "macos").is_err());
    write_file(&development, "");
    assert_eq!(
        agent_deck_for_platform(&desktop, "macos").unwrap(),
        development
    );
    write_file(&packaged, "");
    assert_eq!(
        agent_deck_for_platform(&desktop, "macos").unwrap(),
        packaged
    );
    write_file(&helper, "");
    assert_eq!(agent_deck_for_platform(&desktop, "macos").unwrap(), helper);

    let flat = temp.path().join("Corvid Portable");
    let desktop = flat.join("corvid-desktop.exe");
    let development = flat.join("corvid-agent-deck.exe");
    let packaged = flat.join("plugins/agent-deck/corvid-agent-deck.exe");
    write_file(&development, "");
    assert_eq!(
        agent_deck_for_platform(&desktop, "windows").unwrap(),
        development
    );
    write_file(&packaged, "");
    assert_eq!(
        agent_deck_for_platform(&desktop, "windows").unwrap(),
        packaged
    );
}

#[test]
fn macos_startup_keeps_the_nested_helper_path_relative_to_the_version() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Managed Corvid");
    let helper = Path::new(
        "Corvid.app/Contents/Helpers/Corvid Agent Deck.app/Contents/MacOS/corvid-agent-deck",
    );
    let running = root.join("versions/0.2.0").join(helper);
    write_file(&running, "");
    assert_eq!(unix_startup_executable(&running), running);

    write_file(
        &root.join("install.json"),
        r#"{"schema_version":1,"product":"desktop"}"#,
    );
    let active = root.join("current").join(helper);
    assert_eq!(unix_startup_executable(&running), active);
    assert_eq!(unix_startup_executable(&active), active);

    #[cfg(unix)]
    {
        let pointer = root.join("current");
        std::os::unix::fs::symlink("versions/0.2.0", &pointer).unwrap();
        assert_eq!(
            active.canonicalize().unwrap(),
            running.canonicalize().unwrap()
        );
        let updated = root.join("versions/0.2.1").join(helper);
        write_file(&updated, "");
        fs::remove_file(&pointer).unwrap();
        std::os::unix::fs::symlink("versions/0.2.1", &pointer).unwrap();
        assert_eq!(
            active.canonicalize().unwrap(),
            updated.canonicalize().unwrap()
        );
    }
}

#[test]
fn windows_toast_uses_a_stable_launcher_only_for_windows_setup() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let directory = root.join("versions/0.2.0");
    let plugin = directory.join("plugins/agent-deck/corvid-agent-deck.exe");
    let launcher = root.join("bin/corvid-agent-deck.exe");
    write_file(&plugin, "");
    write_file(&launcher, "");
    assert_eq!(windows_setup_deck_launcher(&plugin), None);
    write_file(&directory.join("native-package"), "windows-setup\r\n");
    assert_eq!(windows_setup_deck_launcher(&plugin), Some(launcher.clone()));
    assert_eq!(
        windows_setup_deck_launcher(&root.join("corvid-agent-deck.exe")),
        None
    );
    fs::remove_file(launcher).unwrap();
    assert_eq!(windows_setup_deck_launcher(&plugin), None);
}
