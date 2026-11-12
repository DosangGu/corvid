use std::time::SystemTime;

use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Phase {
    #[default]
    Idle,
    Checking,
    Current,
    Available,
    Installing(InstallProgress),
    Ready,
}

pub(super) struct Failure {
    pub summary: &'static str,
    pub detail: String,
}

pub(crate) struct State {
    pub(super) installation: Result<Option<Installation>, String>,
    pub(super) phase: Phase,
    pub(super) release: Option<Release>,
    pub(super) installed: Option<Installed>,
    pub(super) last_checked: Option<SystemTime>,
    pub(super) error: Option<Failure>,
    pub(super) details_open: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            installation: Installation::detect()
                .and_then(|installation| {
                    anyhow::ensure!(
                        installation.as_ref().is_none_or(|install| {
                            install.package.product == corvid_update::Product::Desktop
                        }),
                        "Desktop package metadata identifies another product"
                    );
                    Ok(installation)
                })
                .map_err(|error| format!("{error:#}")),
            phase: Phase::Idle,
            release: None,
            installed: None,
            last_checked: None,
            error: None,
            details_open: false,
        }
    }
}

impl State {
    pub(super) fn busy(&self) -> bool {
        matches!(self.phase, Phase::Checking | Phase::Installing(_))
    }

    pub(super) fn start_check(&mut self) -> bool {
        if self.busy() || self.phase == Phase::Ready {
            return false;
        }
        self.phase = Phase::Checking;
        self.release = None;
        self.error = None;
        self.details_open = false;
        true
    }

    pub(super) fn checked(&mut self, result: anyhow::Result<Release>) {
        self.last_checked = Some(SystemTime::now());
        let result = result.and_then(|release| {
            let newer = release.is_newer_than(env!("CARGO_PKG_VERSION"))?;
            Ok((release, newer))
        });
        match result {
            Ok((release, newer)) => {
                self.phase = if newer {
                    Phase::Available
                } else {
                    Phase::Current
                };
                self.release = Some(release);
            }
            Err(error) => {
                self.phase = Phase::Idle;
                self.error = Some(Failure {
                    summary: "Could not check for updates. Check your connection and try again.",
                    detail: format!("{error:#}"),
                });
            }
        }
    }

    pub(super) fn start_install(&mut self) -> Option<(Installation, Release)> {
        if self.phase != Phase::Available {
            return None;
        }
        let installation = self.installation.as_ref().ok()?.as_ref()?;
        if !installation.supports_in_app_update() {
            return None;
        }
        let release = self.release.clone()?;
        let installation = installation.clone();
        self.phase = Phase::Installing(InstallProgress::Downloading);
        self.error = None;
        self.details_open = false;
        Some((installation, release))
    }

    pub(super) fn installed(&mut self, result: anyhow::Result<Installed>) {
        match result {
            Ok(installed) => {
                self.installed = Some(installed);
                self.phase = Phase::Ready;
            }
            Err(error) => {
                self.phase = Phase::Available;
                self.error = Some(Failure {
                    summary: "Could not prepare the update. Try again when you are ready.",
                    detail: format!("{error:#}"),
                });
            }
        }
    }

    pub(super) fn installation_label(&self) -> &'static str {
        match &self.installation {
            Ok(Some(install)) if !install.supports_in_app_update() => "Setup installation",
            Ok(Some(install)) if install.root.is_some() => "Managed installation",
            Ok(Some(_)) => "Portable installation",
            Ok(None) => "Source or externally managed build",
            Err(_) => "Installation could not be identified",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_checks_can_be_retried_without_recreating_the_view() {
        let mut state = State::default();
        assert!(state.start_check());
        assert!(!state.start_check());
        state.checked(Err(anyhow::anyhow!("offline")));
        assert!(!state.busy());
        assert!(state.error.is_some());
        assert!(state.start_check());
        assert!(state.error.is_none());
    }

    #[test]
    fn prepared_updates_are_retained_until_restart() {
        let mut state = State {
            phase: Phase::Installing(InstallProgress::Installing),
            ..State::default()
        };
        assert!(!state.start_check());
        state.installed(Ok(Installed {
            executable: "prepared/corvid".into(),
            message: String::new(),
        }));
        assert_eq!(state.phase, Phase::Ready);
        assert!(!state.start_check());
        assert!(state.installed.is_some());
    }

    #[test]
    fn retries_the_selected_release_and_rejects_parallel_installs() {
        let directory = tempfile::tempdir().unwrap();
        let package = corvid_update::Package {
            schema_version: 1,
            version: "0.1.0".into(),
            source_revision: "a".repeat(40),
            product: corvid_update::Product::Desktop,
            target: corvid_update::target(corvid_update::Product::Desktop)
                .unwrap()
                .into(),
        };
        std::fs::write(
            directory.path().join("package.json"),
            serde_json::to_vec(&package).unwrap(),
        )
        .unwrap();
        let installation = Installation::at(&directory.path().join("corvid")).unwrap();
        let release = Release {
            version: corvid_update::version("0.2.0").unwrap(),
            notes: String::new(),
            manifest: corvid_update::Manifest {
                schema_version: 1,
                version: "0.2.0".into(),
                source_revision: "b".repeat(40),
                assets: Vec::new(),
            },
        };
        let mut state = State {
            installation: Ok(Some(installation)),
            phase: Phase::Available,
            release: Some(release),
            ..State::default()
        };
        assert!(state.start_install().is_some());
        assert!(state.start_install().is_none());
        assert!(!state.start_check());
        state.installed(Err(anyhow::anyhow!("invalid checksum")));
        let (_, retry) = state.start_install().unwrap();
        assert_eq!(retry.version.to_string(), "0.2.0");
        assert_eq!(retry.manifest.source_revision, "b".repeat(40));
    }
}
