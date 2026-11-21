use std::path::PathBuf;

use anyhow::{Result, ensure};
use corvid_client::InteractiveConnection;
use corvid_protocol::{ProtocolFault, WORKSPACE_DIRECTORY_CAPABILITY};

use super::*;
use crate::terminal_session::prepare_destination;

type Query = (u64, BrowseDirectories);
type Updates = mpsc::UnboundedSender<Update>;
type Channels = (
    watch::Sender<Query>,
    mpsc::UnboundedReceiver<Update>,
    mpsc::UnboundedReceiver<Prompt>,
);

pub(super) enum Update {
    Status(String),
    Ready(Prepared),
    SetupFailed(String),
    Listed(u64, Result<DirectoryListing>),
}

impl Draft {
    pub(super) fn update(&mut self, update: Update) -> bool {
        if self.creating.is_some() {
            return false;
        }
        match update {
            Update::Status(status) => self.status = Some(status),
            Update::SetupFailed(error) => {
                self.connecting = false;
                self.status = Some(error);
            }
            Update::Ready(prepared) => {
                let supports_directory = prepared.supports_directory;
                self.field = if prepared.supports_directory {
                    Field::Folders
                } else {
                    Field::Name
                };
                self.prepared = Some(prepared);
                self.connecting = false;
                self.status = None;
                if supports_directory {
                    self.browse_folder("~".into(), None);
                }
            }
            Update::Listed(revision, result) if self.revision == revision => {
                let Some(kind) = self.pending.take() else {
                    return false;
                };
                match result {
                    Ok(listing) => {
                        self.status = None;
                        return self.apply_listing(kind, listing);
                    }
                    Err(error) => self.status = Some(error.to_string()),
                }
            }
            Update::Listed(..) => {}
        }
        false
    }

    fn apply_listing(&mut self, kind: FolderQuery, listing: DirectoryListing) -> bool {
        let reselect = match kind {
            FolderQuery::Validate => {
                self.folder.set(listing.directory);
                self.propose_name(listing.folder_name);
                return true;
            }
            FolderQuery::Complete => None,
            FolderQuery::Browse { reselect } => {
                self.folder.set(listing.directory.clone());
                self.propose_name(listing.folder_name.clone());
                reselect
            }
        };
        self.choice = reselect
            .and_then(|path| listing.entries.iter().position(|entry| entry.path == path))
            .unwrap_or(0);
        self.reveal_folder = true;
        self.listing = Some(listing);
        false
    }
}

pub(super) fn start(
    destination: Destination,
    state_dir: PathBuf,
    known: Option<ControlEndpoint>,
    host: Option<String>,
    store: corvid_client::view_state::ViewStore,
) -> Result<Channels> {
    let (queries, mut pending) = watch::channel((0, BrowseDirectories::default()));
    let (updates, receive) = mpsc::unbounded_channel();
    let (prompts, prompt_receiver) = mpsc::unbounded_channel();
    std::thread::Builder::new().name("corvid-folder-browser".into()).spawn(move || {
        let runtime = match compio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = updates.send(Update::SetupFailed(error.to_string()));
                return;
            }
        };
        let host = host.or_else(|| {
            store.for_target(&destination.saved_target()).map(|saved| saved.host_id.to_string())
        });
        runtime.block_on(async {
            let status = |status: &str| { let _ = updates.send(Update::Status(status.into())); };
            let prompts = Prompts(prompts);
            let setup = async {
                match known {
                    Some(endpoint) => {
                        let link = endpoint.interactive(Vec::new()).await?;
                        Ok((endpoint, link))
                    }
                    None => prepare_destination(
                        &destination, &state_dir, Vec::new(), true, host.as_deref(),
                        &prompts, status,
                    ).await,
                }
            };
            let result = corvid_async::select! {
                _ = updates.closed() => return,
                result = setup => result,
            };
            match result {
                Ok((endpoint, link)) if host.as_deref().is_none_or(|host| host == link.session().host_id) => {
                    let host = link.session().host_id.clone();
                    let supports_directory = link.session().capabilities.iter().any(|name| name == WORKSPACE_DIRECTORY_CAPABILITY);
                    let ready = Prepared { endpoint: endpoint.clone(), host: host.clone(), supports_directory };
                    if updates.send(Update::Ready(ready)).is_ok() && supports_directory {
                        browse(endpoint, host, link, &mut pending, &updates).await;
                    }
                }
                Ok(_) => {
                    let _ = updates.send(Update::SetupFailed("The connection points to a different Corvid host.".into()));
                }
                Err(error) => {
                    let detail = if daemon_prompt::has_fault(&error, "HostKeyRequired") {
                        "Verify the host key with OpenSSH and try again.".into()
                    } else if daemon_prompt::has_fault(&error, "AuthenticationRequired") {
                        "Check the SSH alias and key in your OpenSSH configuration, then try again.".into()
                    } else {
                        error.to_string()
                    };
                    let _ = updates.send(Update::SetupFailed(detail));
                }
            }
        });
    })?;
    Ok((queries, receive, prompt_receiver))
}

async fn browse(
    endpoint: ControlEndpoint,
    host: String,
    mut link: InteractiveConnection,
    pending: &mut watch::Receiver<Query>,
    updates: &Updates,
) {
    while pending.changed().await.is_ok() {
        // Let a burst of edits settle before sending the latest prefix.
        corvid_async::time::sleep(Duration::from_millis(150)).await;
        let (revision, query) = pending.borrow_and_update().clone();
        let result = corvid_async::select! {
            _ = updates.closed() => return,
            result = list(&endpoint, &host, &mut link, query) => result,
        };
        if updates.send(Update::Listed(revision, result)).is_err() {
            break;
        }
    }
}

async fn list(
    endpoint: &ControlEndpoint,
    host: &str,
    link: &mut InteractiveConnection,
    query: BrowseDirectories,
) -> Result<DirectoryListing> {
    match link.browse_directories(query.clone()).await {
        Err(error) if error.downcast_ref::<ProtocolFault>().is_none() => {
            let next = endpoint
                .interactive(vec![WORKSPACE_DIRECTORY_CAPABILITY.into()])
                .await?;
            ensure!(
                next.session().host_id == host,
                "The connection points to a different Corvid host."
            );
            *link = next;
            link.browse_directories(query).await
        }
        result => result,
    }
}
