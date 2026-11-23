use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use corvid_client::AttachOptions;
use corvid_protocol::{ProtocolFault, wire::WorkspaceInfo};
use uuid::Uuid;

pub(super) async fn list(runtime: Option<PathBuf>) -> Result<u32> {
    let runtime = corvid_client::view_state::tui_runtime(runtime)?;
    for workspace in workspaces(&runtime).await? {
        println!(
            "{}\t{}",
            workspace.workspace_id,
            workspace.label.escape_debug()
        );
    }
    Ok(0)
}

pub(super) async fn create(runtime: Option<PathBuf>, name: String) -> Result<u32> {
    corvid_client::check_tty()?;
    let directory = std::env::current_dir()?
        .into_os_string()
        .into_string()
        .ok()
        .context("the current directory is not valid UTF-8")?;
    let runtime = corvid_client::view_state::tui_runtime(runtime)?;
    let options = AttachOptions {
        remember_view: true,
        launch: Some(Vec::new()),
        new_workspace: Some(name),
        workspace_directory: Some(directory),
        ..AttachOptions::default()
    };
    #[cfg(windows)]
    {
        super::windows::start_attach(runtime, options).await
    }
    #[cfg(unix)]
    {
        super::daemon_bootstrap::ensure_cli(&runtime).await?;
        super::attach_monitored(&runtime, options).await
    }
}

pub(super) async fn attach_target(
    runtime: &Path,
    target: Option<String>,
    workspace: Option<String>,
) -> Result<Option<String>> {
    let Some(target) = target else {
        return Ok(workspace);
    };
    corvid_client::check_tty()?;
    let workspaces = workspaces(runtime).await?;
    resolve(&workspaces, &target).map(|workspace| Some(workspace.workspace_id.clone()))
}

pub(super) async fn attach_existing(runtime: &Path, mut options: AttachOptions) -> Result<u32> {
    options.remember_view = true;
    #[cfg(unix)]
    let result = {
        let start_if_missing = options.workspace.is_none() && options.launch.is_none();
        match super::attach_monitored(runtime, options.clone()).await {
            Err(error) if start_if_missing && corvid_client::daemon_missing(&error) => {
                super::daemon_bootstrap::ensure_cli(runtime).await?;
                super::attach_monitored(runtime, options).await
            }
            result => result,
        }
    };
    #[cfg(windows)]
    let result = super::windows::start_attach(runtime.to_owned(), options).await;

    match result {
        Err(error)
            if error
                .downcast_ref::<ProtocolFault>()
                .is_some_and(|fault| fault.code == "NoTerminal") =>
        {
            Err(error.context("Use corvid to start work or corvid new NAME to create a workspace"))
        }
        result => result,
    }
}

async fn workspaces(runtime: &Path) -> Result<Vec<WorkspaceInfo>> {
    match corvid_client::workspaces(runtime).await {
        Ok(list) => Ok(list.workspaces),
        Err(error) if corvid_client::daemon_missing(&error) => Err(error
            .context("The Corvid daemon is not running. Run corvid or corvid ensure-daemon first")),
        Err(error) => Err(error),
    }
}

fn resolve<'a>(workspaces: &'a [WorkspaceInfo], target: &str) -> Result<&'a WorkspaceInfo> {
    if let Ok(id) = Uuid::parse_str(target)
        && let Some(workspace) = workspaces
            .iter()
            .find(|workspace| Uuid::parse_str(&workspace.workspace_id) == Ok(id))
    {
        return Ok(workspace);
    }
    let mut matches = workspaces
        .iter()
        .filter(|workspace| workspace.label == target);
    let Some(first) = matches.next() else {
        bail!("Workspace {target:?} was not found. Run corvid list to see available workspaces");
    };
    let ids = std::iter::once(first)
        .chain(matches)
        .map(|workspace| workspace.workspace_id.as_str())
        .collect::<Vec<_>>();
    if ids.len() > 1 {
        bail!(
            "Multiple workspaces match {target:?}. Run corvid attach with one of these IDs: {}",
            ids.join(", ")
        );
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_exact_names_and_prioritizes_matching_ids() {
        let id = Uuid::new_v4().to_string();
        let workspaces = [
            WorkspaceInfo {
                workspace_id: id.clone(),
                label: "Code review".into(),
                ..Default::default()
            },
            WorkspaceInfo {
                workspace_id: Uuid::new_v4().to_string(),
                label: id.clone(),
                ..Default::default()
            },
        ];
        assert_eq!(
            resolve(&workspaces, "Code review").unwrap().workspace_id,
            id
        );
        assert_eq!(resolve(&workspaces, &id).unwrap().workspace_id, id);
        assert!(resolve(&workspaces, "code review").is_err());
    }

    #[test]
    fn shows_both_ids_for_duplicate_names() {
        let workspaces = ["first", "second"].map(|id| WorkspaceInfo {
            workspace_id: id.into(),
            label: "review".into(),
            ..Default::default()
        });
        let error = resolve(&workspaces, "review").unwrap_err().to_string();
        assert!(error.contains("first, second"), "{error}");
    }
}
