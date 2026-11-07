use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const MAX_WORKSPACES: usize = 32;
const MAX_TABS: usize = 128;
const MAX_NODES: usize = 1024;
const MAX_DEPTH: usize = 32;
const MAX_LABEL: usize = 1024;
const MAX_PATH: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layout {
    pub workspaces: Vec<Workspace>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub label: String,
    pub tabs: Vec<Tab>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tab {
    pub id: Uuid,
    pub label: String,
    pub root: Node,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    Pane {
        id: Uuid,
        last_cwd: Option<CwdObservation>,
    },
    Split {
        axis: Axis,
        children: Vec<Node>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CwdObservation {
    pub path: Vec<u8>,
    pub observed_unix_ms: u64,
    pub source: CwdSource,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CwdSource {
    LaunchDirectory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub workspace: Uuid,
    pub tab: Uuid,
    pub pane: Uuid,
}

#[derive(Clone, Copy, Debug, Error)]
#[error("layout has invalid IDs, labels, paths, or tree bounds")]
pub struct InvalidLayout;

impl Layout {
    pub fn empty_terminal() -> Self {
        Self {
            workspaces: vec![Workspace {
                id: Uuid::new_v4(),
                label: "Main".to_owned(),
                tabs: vec![Tab {
                    id: Uuid::new_v4(),
                    label: "Shell".to_owned(),
                    root: Node::Pane {
                        id: Uuid::new_v4(),
                        last_cwd: None,
                    },
                }],
            }],
        }
    }

    pub fn validate(&self) -> Result<(), InvalidLayout> {
        if self.workspaces.is_empty() || self.workspaces.len() > MAX_WORKSPACES {
            return Err(InvalidLayout);
        }
        let mut limits = Limits::default();
        for workspace in &self.workspaces {
            limits.id(workspace.id)?;
            validate_label(&workspace.label)?;
            limits.tabs += workspace.tabs.len();
            if workspace.tabs.is_empty() || limits.tabs > MAX_TABS {
                return Err(InvalidLayout);
            }
            for tab in &workspace.tabs {
                limits.id(tab.id)?;
                validate_label(&tab.label)?;
                limits.node(&tab.root, 1)?;
            }
        }
        Ok(())
    }

    pub fn primary(&self) -> Option<Location> {
        let workspace = self.workspaces.first()?;
        let tab = workspace.tabs.first()?;
        let pane = tab.root.first_pane()?;
        Some(Location {
            workspace: workspace.id,
            tab: tab.id,
            pane,
        })
    }

    pub fn locate(&self, workspace: Option<Uuid>, tab: Option<Uuid>) -> Option<Location> {
        let workspace = match workspace {
            Some(id) => self
                .workspaces
                .iter()
                .find(|workspace| workspace.id == id)?,
            None => self.workspaces.first()?,
        };
        let tab = match tab {
            Some(id) => workspace.tabs.iter().find(|tab| tab.id == id)?,
            None => workspace.tabs.first()?,
        };
        Some(Location {
            workspace: workspace.id,
            tab: tab.id,
            pane: tab.root.first_pane()?,
        })
    }

    pub fn add_workspace(
        &mut self,
        label: String,
        tab_label: String,
    ) -> Result<Location, InvalidLayout> {
        validate_label(&label)?;
        if self.workspaces.len() >= MAX_WORKSPACES {
            return Err(InvalidLayout);
        }
        let workspace = Uuid::new_v4();
        let (tab, pane) = self.make_tab(tab_label)?;
        let location = Location {
            workspace,
            tab: tab.id,
            pane,
        };
        self.workspaces.push(Workspace {
            id: workspace,
            label,
            tabs: vec![tab],
        });
        Ok(location)
    }

    pub fn add_tab(&mut self, workspace: Uuid, label: String) -> Result<Location, InvalidLayout> {
        let (tab, pane) = self.make_tab(label)?;
        let location = Location {
            workspace,
            tab: tab.id,
            pane,
        };
        self.workspaces
            .iter_mut()
            .find(|item| item.id == workspace)
            .ok_or(InvalidLayout)?
            .tabs
            .push(tab);
        Ok(location)
    }

    fn make_tab(&self, label: String) -> Result<(Tab, Uuid), InvalidLayout> {
        validate_label(&label)?;
        let tabs = self.workspaces.iter().flat_map(|workspace| &workspace.tabs);
        if tabs.clone().count() >= MAX_TABS
            || tabs.map(|tab| tab.root.node_count()).sum::<usize>() >= MAX_NODES
        {
            return Err(InvalidLayout);
        }
        let pane = Uuid::new_v4();
        Ok((
            Tab {
                id: Uuid::new_v4(),
                label,
                root: Node::Pane {
                    id: pane,
                    last_cwd: None,
                },
            },
            pane,
        ))
    }

    pub fn last_cwd(&self, location: Location) -> Option<&CwdObservation> {
        let workspace = self
            .workspaces
            .iter()
            .find(|item| item.id == location.workspace)?;
        let tab = workspace.tabs.iter().find(|item| item.id == location.tab)?;
        tab.root.cwd(location.pane)
    }

    pub fn record_cwd(
        &mut self,
        location: Location,
        cwd: CwdObservation,
    ) -> Result<(), InvalidLayout> {
        validate_path(&cwd.path)?;
        let workspace = self
            .workspaces
            .iter_mut()
            .find(|item| item.id == location.workspace)
            .ok_or(InvalidLayout)?;
        let tab = workspace
            .tabs
            .iter_mut()
            .find(|item| item.id == location.tab)
            .ok_or(InvalidLayout)?;
        *tab.root.cwd_mut(location.pane).ok_or(InvalidLayout)? = Some(cwd);
        Ok(())
    }
}

impl Node {
    fn node_count(&self) -> usize {
        match self {
            Self::Pane { .. } => 1,
            Self::Split { children, .. } => {
                1 + children.iter().map(Self::node_count).sum::<usize>()
            }
        }
    }

    fn first_pane(&self) -> Option<Uuid> {
        match self {
            Self::Pane { id, .. } => Some(*id),
            Self::Split { children, .. } => children.iter().find_map(Self::first_pane),
        }
    }

    pub fn pane_ids(&self, output: &mut Vec<String>) {
        match self {
            Self::Pane { id, .. } => output.push(id.to_string()),
            Self::Split { children, .. } => {
                for child in children {
                    child.pane_ids(output);
                }
            }
        }
    }

    fn cwd(&self, pane: Uuid) -> Option<&CwdObservation> {
        match self {
            Self::Pane { id, last_cwd } if *id == pane => last_cwd.as_ref(),
            Self::Pane { .. } => None,
            Self::Split { children, .. } => children.iter().find_map(|child| child.cwd(pane)),
        }
    }

    fn cwd_mut(&mut self, pane: Uuid) -> Option<&mut Option<CwdObservation>> {
        match self {
            Self::Pane { id, last_cwd } if *id == pane => Some(last_cwd),
            Self::Pane { .. } => None,
            Self::Split { children, .. } => {
                children.iter_mut().find_map(|child| child.cwd_mut(pane))
            }
        }
    }
}

#[derive(Default)]
struct Limits {
    ids: HashSet<Uuid>,
    tabs: usize,
    nodes: usize,
}

impl Limits {
    fn id(&mut self, id: Uuid) -> Result<(), InvalidLayout> {
        if id.is_nil() || !self.ids.insert(id) {
            return Err(InvalidLayout);
        }
        Ok(())
    }

    fn node(&mut self, node: &Node, depth: usize) -> Result<(), InvalidLayout> {
        self.nodes += 1;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err(InvalidLayout);
        }
        match node {
            Node::Pane { id, last_cwd } => {
                self.id(*id)?;
                if let Some(cwd) = last_cwd {
                    validate_path(&cwd.path)?;
                }
            }
            Node::Split { children, .. } => {
                if children.len() < 2 {
                    return Err(InvalidLayout);
                }
                for child in children {
                    self.node(child, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}

fn validate_label(label: &str) -> Result<(), InvalidLayout> {
    if label.is_empty() || label.len() > MAX_LABEL || label.chars().any(char::is_control) {
        return Err(InvalidLayout);
    }
    Ok(())
}

fn validate_path(path: &[u8]) -> Result<(), InvalidLayout> {
    if path.first() != Some(&b'/') || path.len() > MAX_PATH || path.contains(&0) {
        return Err(InvalidLayout);
    }
    Ok(())
}
