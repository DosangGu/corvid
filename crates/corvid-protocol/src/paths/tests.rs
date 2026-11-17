use super::*;

const FIRST_BOOT: &str = "01234567-89ab-cdef-0123-456789abcdef";
const NEXT_BOOT: &str = "76543210-abcd-ef01-2345-6789abcdef01";
const CONTROLLER: Uuid = Uuid::from_u128(0x01234567_89ab_cdef_0123_456789abcdef);

#[derive(Default)]
struct Inputs<'a> {
    env: &'a [(&'a str, &'a str)],
    boot: Option<&'a str>,
    user_runtime: Option<&'a str>,
}

impl PathInputs for Inputs<'_> {
    fn var(&self, name: &str) -> Option<OsString> {
        self.env
            .iter()
            .find_map(|(key, value)| (*key == name).then(|| OsString::from(value)))
    }

    fn boot_session(&self) -> io::Result<String> {
        self.boot
            .map(str::to_owned)
            .context("no boot ID")
            .map_err(io::Error::other)
    }

    fn user_runtime(&self) -> Option<PathBuf> {
        self.user_runtime.map(PathBuf::from)
    }
}

fn state_roles<P: PlatformPaths>(inputs: &Inputs<'_>, expected: [&str; 4]) {
    let roles = [
        StateRole::Daemon,
        StateRole::Companion,
        StateRole::AgentDeck,
        StateRole::TuiViews,
    ];
    for (role, expected) in roles.into_iter().zip(expected) {
        assert_eq!(P::state(inputs, role, None).unwrap(), Path::new(expected));
        let explicit = PathBuf::from("/explicit");
        assert_eq!(
            P::state(&Inputs::default(), role, Some(explicit.clone())).unwrap(),
            explicit
        );
    }
}

#[test]
fn existing_persistent_locations_need_no_migration() {
    state_roles::<Linux>(
        &Inputs {
            env: &[("XDG_STATE_HOME", "/state"), ("HOME", "/home")],
            ..Default::default()
        },
        [
            "/state/corvid",
            "/state/corvid/companion",
            "/state/corvid/companion",
            "/state/corvid/clients/tui",
        ],
    );
    state_roles::<Linux>(
        &Inputs {
            env: &[("XDG_STATE_HOME", ""), ("HOME", "/home")],
            ..Default::default()
        },
        [
            "/home/.local/state/corvid",
            "/home/.local/state/corvid/companion",
            "/home/.local/state/corvid/companion",
            "/home/.local/state/corvid/clients/tui",
        ],
    );
    state_roles::<MacOs>(
        &Inputs {
            env: &[("HOME", "/home"), ("XDG_STATE_HOME", "/ignored")],
            ..Default::default()
        },
        [
            "/home/Library/Application Support/Corvid/daemon",
            "/home/Library/Application Support/Corvid/companion",
            "/home/Library/Application Support/Corvid/companion",
            "/home/Library/Application Support/Corvid/clients/tui",
        ],
    );
    state_roles::<Windows>(
        &Inputs {
            env: &[("LOCALAPPDATA", "/appdata")],
            ..Default::default()
        },
        [
            "/appdata/Corvid/daemon",
            "/appdata/Corvid/companion",
            "/appdata/Corvid/companion",
            "/appdata/Corvid/clients/tui",
        ],
    );
}

#[test]
fn state_roles_preserve_missing_and_empty_environment_behavior() {
    for env in [
        &[][..],
        &[("HOME", ""), ("XDG_STATE_HOME", ""), ("LOCALAPPDATA", "")][..],
    ] {
        let inputs = Inputs {
            env,
            ..Default::default()
        };
        for role in [StateRole::Daemon, StateRole::Companion, StateRole::TuiViews] {
            assert!(Linux::state(&inputs, role, None).is_err());
            assert!(MacOs::state(&inputs, role, None).is_err());
            assert!(Windows::state(&inputs, role, None).is_err());
        }
        assert!(MacOs::state(&inputs, StateRole::AgentDeck, None).is_err());
        assert!(Windows::state(&inputs, StateRole::AgentDeck, None).is_err());
    }
    assert!(Linux::state(&Inputs::default(), StateRole::AgentDeck, None).is_err());
    let inputs = Inputs {
        env: &[("HOME", "")],
        ..Default::default()
    };
    assert_eq!(
        Linux::state(&inputs, StateRole::AgentDeck, None).unwrap(),
        Path::new(".local/state/corvid/companion"),
    );
}

fn view_override<P: PlatformPaths>() {
    for (value, expected) in [("/custom", "/custom/tui"), ("", "tui")] {
        let env = [(VIEW_STATE_DIR, value)];
        let inputs = Inputs {
            env: &env,
            ..Default::default()
        };
        assert_eq!(
            P::state(&inputs, StateRole::TuiViews, None).unwrap(),
            Path::new(expected)
        );
        assert!(P::state(&inputs, StateRole::Daemon, None).is_err());
    }
}

#[test]
fn view_override_is_a_base_only_for_tui_views() {
    view_override::<Linux>();
    view_override::<MacOs>();
    view_override::<Windows>();
}

fn runtime_precedence<P: PlatformPaths>(env: &[(&str, &str)], expected_default: &str) {
    let inputs = Inputs {
        env,
        boot: Some(FIRST_BOOT),
        ..Default::default()
    };
    assert_eq!(
        P::runtime(&inputs, None, || None).unwrap().path,
        Path::new(expected_default),
    );
    assert_eq!(
        P::runtime(&inputs, None, || Some(PathBuf::from("/previous")))
            .unwrap()
            .path,
        Path::new("/previous"),
    );
    for value in ["/custom", ""] {
        let inputs = Inputs {
            env: &[(crate::collector::RUNTIME_DIR, value)],
            ..Default::default()
        };
        let previous = || panic!("a runtime override should bypass view storage");
        let resolved = P::runtime(&inputs, None, previous).unwrap();
        assert_eq!(resolved.path, Path::new(value));
        assert!(resolved.boot_parent.is_none());
        assert_eq!(
            P::runtime(&inputs, Some(PathBuf::from("/cli")), previous)
                .unwrap()
                .path,
            Path::new("/cli"),
        );
    }
}

#[test]
fn runtime_precedence_preserves_overrides_and_saved_navigation() {
    runtime_precedence::<Linux>(&[("XDG_RUNTIME_DIR", "/run")], "/run/corvid");
    runtime_precedence::<MacOs>(&[("HOME", "/home")], "/home/.corvid/r-01234567-89ab-cd");
    runtime_precedence::<Windows>(&[("LOCALAPPDATA", "/appdata")], "/appdata/Corvid/runtime");

    for env in [
        &[][..],
        &[("HOME", ""), ("XDG_RUNTIME_DIR", ""), ("LOCALAPPDATA", "")][..],
    ] {
        let inputs = Inputs {
            env,
            ..Default::default()
        };
        assert!(Linux::runtime(&inputs, None, || None).is_err());
        assert!(MacOs::runtime(&inputs, None, || None).is_err());
        assert!(Windows::runtime(&inputs, None, || None).is_err());
    }
}

#[test]
fn linux_presentation_uses_xdg_directly_and_only_monitor_has_fallbacks() {
    use UnixEndpoint::*;

    let local = Some(Path::new("/local-daemon"));
    let cases = [
        ("/xdg", Some("/run/user/42"), local, "/xdg"),
        ("", Some("/run/user/42"), local, "/run/user/42"),
        ("", None, local, "/local-daemon"),
        ("", None, None, "/override"),
    ];
    for (xdg, user_runtime, local, expected) in cases {
        let inputs = Inputs {
            env: &[
                ("XDG_RUNTIME_DIR", xdg),
                (crate::collector::RUNTIME_DIR, "/override"),
            ],
            user_runtime,
            ..Default::default()
        };
        for (role, name) in [
            (Monitor(None), "corvid-companion"),
            (
                Monitor(Some(CONTROLLER)),
                "cv-slot-0123456789abcdef0123456789abcdef",
            ),
        ] {
            let path = Linux::endpoint(&inputs, role, local).unwrap().path;
            assert_eq!(path, Path::new(expected).join(name));
        }
        for (role, name) in [
            (Companion, "corvid-companion"),
            (
                AgentDeck(CONTROLLER),
                "cv-deck-0123456789abcdef0123456789abcdef",
            ),
            (
                DesktopFocus(CONTROLLER),
                "cv-deck-f0123456789abcdef0123456789abcdef",
            ),
        ] {
            let path = Linux::endpoint(&inputs, role, None);
            if xdg.is_empty() {
                assert!(path.is_err());
            } else {
                assert_eq!(path.unwrap().path, Path::new(xdg).join(name));
            }
        }
    }
    assert!(Linux::endpoint(&Inputs::default(), Monitor(None), None).is_err());
}

#[test]
fn macos_endpoints_follow_boot_and_keep_controller_scope() {
    use UnixEndpoint::*;

    let roles = [
        (Companion, "corvid-companion"),
        (Monitor(None), "corvid-companion"),
        (
            Monitor(Some(CONTROLLER)),
            "s-0123456789abcdef0123456789abcdef",
        ),
        (AgentDeck(CONTROLLER), "d-0123456789abcdef0123456789abcdef"),
        (
            DesktopFocus(CONTROLLER),
            "d-f0123456789abcdef0123456789abcdef",
        ),
    ];
    for (boot, base) in [
        (FIRST_BOOT, "/home/.corvid/r-01234567-89ab-cd"),
        (NEXT_BOOT, "/home/.corvid/r-76543210-abcd-ef"),
    ] {
        let inputs = Inputs {
            env: &[("HOME", "/home")],
            boot: Some(boot),
            ..Default::default()
        };
        for (role, name) in roles {
            let resolved = MacOs::endpoint(&inputs, role, Some(Path::new("/ignored"))).unwrap();
            assert_eq!(resolved.path, Path::new(base).join(name));
            assert_eq!(
                resolved.boot_parent.as_deref(),
                Some(Path::new("/home/.corvid"))
            );
        }
    }
    let inputs = Inputs {
        env: &[(crate::collector::RUNTIME_DIR, "/override")],
        ..Default::default()
    };
    for (role, name) in roles {
        let resolved = MacOs::endpoint(&inputs, role, None).unwrap();
        assert_eq!(resolved.path, Path::new("/override").join(name));
        assert!(resolved.boot_parent.is_none());
    }
    for boot in [
        None,
        Some("too-short"),
        Some("☃0000000-0000-0000-0000-0000000000"),
    ] {
        let inputs = Inputs {
            env: &[("HOME", "/home")],
            boot,
            ..Default::default()
        };
        assert!(MacOs::runtime(&inputs, None, || None).is_err());
    }
}

#[test]
fn macos_login_startup_resolves_saved_socket_names_in_the_current_boot() {
    let inputs = Inputs {
        env: &[("HOME", "/home")],
        boot: Some(NEXT_BOOT),
        ..Default::default()
    };
    for address in [
        "/home/.corvid/r-01234567-89ab-cd/corvid-companion",
        "/home/.corvid/r-76543210-abcd-ef/corvid-companion",
    ] {
        let name = startup_socket_name(&inputs, Path::new(address))
            .unwrap()
            .unwrap();
        assert_eq!(
            named_socket(&inputs, name).unwrap().path,
            Path::new("/home/.corvid/r-76543210-abcd-ef/corvid-companion")
        );
    }
    for explicit in ["/custom/corvid-companion", "/home/.corvid/r-custom/scope"] {
        assert_eq!(
            startup_socket_name(&inputs, Path::new(explicit)).unwrap(),
            None
        );
    }
    for invalid in ["../escape", "", ".", "..", "dir/scope"] {
        assert!(named_socket(&inputs, OsStr::new(invalid)).is_err());
    }
}

#[test]
fn windows_presentation_pipe_instances_are_compatible() {
    assert_eq!(monitor_pipe_instance("custom", None), "custom");
    let instance = "0123456789abcdef0123456789abcdef";
    assert_eq!(monitor_pipe_instance("custom", Some(CONTROLLER)), instance);
    assert_eq!(controller_pipe_instance(CONTROLLER), instance);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn companion_cli_preserves_explicit_socket_directory() {
    for explicit in ["/custom/companion", ""] {
        let path = PathBuf::from(explicit);
        assert_eq!(companion_socket(Some(path.clone())).unwrap().path, path);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_resolution_is_read_only_and_preparation_checks_the_boot_parent() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let home = tempfile::tempdir_in(std::env::home_dir().unwrap()).unwrap();
    let inputs = Inputs {
        env: &[("HOME", home.path().to_str().unwrap())],
        boot: Some(FIRST_BOOT),
        ..Default::default()
    };
    let parent = home.path().join(".corvid");
    let runtime = MacOs::runtime(&inputs, None, || None).unwrap();
    assert!(!parent.exists());
    assert!(!runtime.prepare().unwrap().exists());
    assert_eq!(
        parent.metadata().unwrap().permissions().mode() & 0o777,
        0o700
    );

    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(
        MacOs::runtime(&inputs, None, || None)
            .unwrap()
            .prepare()
            .is_err()
    );
    std::fs::remove_dir(&parent).unwrap();
    let other = home.path().join("other");
    std::fs::create_dir(&other).unwrap();
    symlink(other, parent).unwrap();
    assert!(
        MacOs::runtime(&inputs, None, || None)
            .unwrap()
            .prepare()
            .is_err()
    );
}
