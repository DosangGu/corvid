use std::collections::BTreeSet;
use std::{env, fs};

use super::*;

const MAX_DIRECTORY_ENTRIES: usize = 512;
const MAX_GUI_SOCKETS: usize = 64;
const MAX_PANES: usize = 128;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct WezTermCandidate {
    registration: WezTermRegistration,
    title: String,
}

impl WezTermCandidate {
    pub fn location(&self) -> WezTermLocation {
        self.registration.location()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn target(&self) -> WezTermTarget {
        WezTermTarget {
            gui_socket: self
                .registration
                .gui
                .socket
                .to_str()
                .expect("verified UTF-8 socket path")
                .to_owned(),
            pane_id: Some(self.registration.location.pane_id),
            selection: Some(self.registration.selection()),
        }
    }
}

#[derive(Deserialize)]
struct ListedPane {
    #[serde(flatten)]
    location: PaneLocation,
    #[serde(default)]
    title: String,
}

pub async fn discover_wezterm_panes() -> Result<Vec<WezTermCandidate>> {
    timeout(DISCOVERY_TIMEOUT, discover_at(socket_paths()?))
        .await
        .context("WezTerm discovery timed out")?
}

pub(super) fn socket_paths() -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    if let Some(socket) = env::var_os("WEZTERM_UNIX_SOCKET") {
        paths.insert(PathBuf::from(socket));
    }
    let Some(home) = env::home_dir() else {
        return Ok(paths);
    };
    let directory = local_path(&home.join(r".local\share\wezterm"))?;
    let _guard = match pin_path(&directory, true) {
        Ok(guard) => guard,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(paths);
        }
        Err(error) => return Err(error),
    };
    for (index, entry) in fs::read_dir(directory)?.enumerate() {
        ensure!(
            index < MAX_DIRECTORY_ENTRIES,
            "too many WezTerm runtime entries"
        );
        let path = entry?.path();
        if socket_pid(&path).is_ok() {
            paths.insert(path);
            ensure!(paths.len() <= MAX_GUI_SOCKETS, "too many WezTerm sockets");
        }
    }
    Ok(paths)
}

pub(super) async fn discover_at(
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<WezTermCandidate>> {
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for path in paths {
        let Ok(gui) = Gui::open(&path) else {
            continue;
        };
        let identity = (gui.identity.pid, gui.identity.created);
        if seen.contains(&identity) {
            continue;
        }
        let Ok(bytes) = gui.invoke(&["list", "--format", "json"]).await else {
            continue;
        };
        seen.insert(identity);
        let panes: Vec<ListedPane> = serde_json::from_slice(&bytes)?;
        let mut pane_ids = BTreeSet::new();
        for pane in panes {
            ensure!(
                pane_ids.insert(pane.location.pane_id),
                "ambiguous WezTerm pane ID"
            );
            ensure!(candidates.len() < MAX_PANES, "too many WezTerm panes");
            candidates.push(WezTermCandidate {
                registration: WezTermRegistration {
                    gui: gui.identity.clone(),
                    location: pane.location,
                },
                title: pane
                    .title
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(80)
                    .collect(),
            });
        }
    }
    candidates.sort_by_key(|candidate| {
        let location = candidate.location();
        (
            location.gui_pid,
            location.window_id,
            location.tab_id,
            location.pane_id,
        )
    });
    Ok(candidates)
}
