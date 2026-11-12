use std::collections::{HashMap, HashSet};

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
    #[serde(default)]
    pub manual_name: Option<String>,
    pub root: Node,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    Pane {
        id: Uuid,
        last_cwd: Option<CwdObservation>,
        #[serde(default)]
        manual_name: Option<String>,
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
    ProcessDirectory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub workspace: Uuid,
    pub tab: Uuid,
    pub pane: Uuid,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CloseTarget {
    Workspace(Uuid),
    Tab { workspace: Uuid, tab: Uuid },
    Pane(Location),
}

impl CloseTarget {
    pub fn workspace(self) -> Uuid {
        match self {
            Self::Workspace(workspace) | Self::Tab { workspace, .. } => workspace,
            Self::Pane(location) => location.workspace,
        }
    }

    pub fn contains(self, location: Location) -> bool {
        match self {
            Self::Workspace(workspace) => workspace == location.workspace,
            Self::Tab { workspace, tab } => workspace == location.workspace && tab == location.tab,
            Self::Pane(pane) => pane == location,
        }
    }
}

#[derive(Clone, Copy, Debug, Error)]
#[error("layout has invalid IDs, labels, paths, or tree bounds")]
pub struct InvalidLayout;

impl Layout {
    pub fn rename(
        &mut self,
        location: Location,
        pane: bool,
        name: Option<String>,
    ) -> Result<(), InvalidLayout> {
        if let Some(name) = &name {
            validate_label(name)?;
        }
        let tab = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == location.workspace)
            .and_then(|workspace| workspace.tabs.iter_mut().find(|tab| tab.id == location.tab))
            .ok_or(InvalidLayout)?;
        if pane {
            let Node::Pane { manual_name, .. } =
                tab.root.pane_mut(location.pane, 1).ok_or(InvalidLayout)?
            else {
                return Err(InvalidLayout);
            };
            *manual_name = name;
        } else {
            tab.manual_name = name;
        }
        Ok(())
    }

    pub fn display(&self, titles: &HashMap<Uuid, String>) -> Self {
        let mut result = self.clone();
        for workspace in &mut result.workspaces {
            for (index, tab) in workspace.tabs.iter_mut().enumerate() {
                tab.label = tab.display_name(index, titles);
            }
        }
        result
    }

    pub fn pane_title(&self, location: Location, titles: &HashMap<Uuid, String>) -> Option<String> {
        let tab = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == location.workspace)?
            .tabs
            .iter()
            .find(|tab| tab.id == location.tab)?;
        let mut panes = Vec::new();
        tab.root.named_panes(&mut panes);
        let (index, (_, manual)) = panes
            .iter()
            .enumerate()
            .find(|(_, (id, _))| *id == location.pane)?;
        Some(pane_name(location.pane, *manual, index, titles))
    }

    pub fn empty_terminal() -> Self {
        Self {
            workspaces: vec![Workspace {
                id: Uuid::new_v4(),
                label: "Main".to_owned(),
                tabs: vec![Tab {
                    id: Uuid::new_v4(),
                    label: "Shell".to_owned(),
                    manual_name: None,
                    root: Node::Pane {
                        id: Uuid::new_v4(),
                        last_cwd: None,
                        manual_name: None,
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
                if let Some(name) = &tab.manual_name {
                    validate_label(name)?;
                }
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
        let (mut tab, pane) = self.make_tab(tab_label)?;
        tab.manual_name = None;
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

    pub fn contains(&self, location: Location) -> bool {
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == location.workspace)
            .and_then(|workspace| workspace.tabs.iter().find(|tab| tab.id == location.tab))
            .is_some_and(|tab| tab.root.contains(location.pane))
    }

    pub fn close(&mut self, target: CloseTarget) -> Result<Vec<Location>, InvalidLayout> {
        let workspace = self
            .workspaces
            .iter()
            .position(|item| item.id == target.workspace())
            .ok_or(InvalidLayout)?;
        let removed: Vec<_> = self.workspaces[workspace]
            .tabs
            .iter()
            .flat_map(|tab| self.panes(target.workspace(), tab.id))
            .filter(|location| target.contains(*location))
            .collect();
        let first = removed.first().ok_or(InvalidLayout)?;

        if !matches!(target, CloseTarget::Workspace(_)) {
            let tabs = &mut self.workspaces[workspace].tabs;
            let index = tabs
                .iter()
                .position(|tab| tab.id == first.tab)
                .ok_or(InvalidLayout)?;
            if let CloseTarget::Pane(location) = target
                && tabs[index].root.remove_child_pane(location.pane)
            {
                return Ok(removed);
            }
            tabs.remove(index);
            if !tabs.is_empty() {
                return Ok(removed);
            }
        }

        self.workspaces.remove(workspace);
        if self.workspaces.is_empty() {
            *self = Self::empty_terminal();
        }
        Ok(removed)
    }

    pub fn panes(&self, workspace: Uuid, tab: Uuid) -> Vec<Location> {
        let mut panes = Vec::new();
        if let Some(tab) = self
            .workspaces
            .iter()
            .find(|item| item.id == workspace)
            .and_then(|workspace| workspace.tabs.iter().find(|item| item.id == tab))
        {
            tab.root.visit_panes(&mut |pane| {
                panes.push(Location {
                    workspace,
                    tab: tab.id,
                    pane,
                })
            });
        }
        panes
    }

    pub fn split_pane(
        &mut self,
        location: Location,
        axis: Axis,
    ) -> Result<Location, InvalidLayout> {
        let nodes: usize = self
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .map(|tab| tab.root.node_count())
            .sum();
        if nodes + 2 > MAX_NODES {
            return Err(InvalidLayout);
        }
        let tab = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == location.workspace)
            .and_then(|workspace| workspace.tabs.iter_mut().find(|tab| tab.id == location.tab))
            .ok_or(InvalidLayout)?;
        let target = tab.root.pane_mut(location.pane, 1).ok_or(InvalidLayout)?;
        let pane = Uuid::new_v4();
        let original = target.clone();
        *target = Node::Split {
            axis,
            children: vec![
                original,
                Node::Pane {
                    id: pane,
                    last_cwd: None,
                    manual_name: None,
                },
            ],
        };
        Ok(Location { pane, ..location })
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

    pub fn move_pane_tab(
        &mut self,
        source: Location,
        destination: Uuid,
    ) -> Result<Location, InvalidLayout> {
        let from = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == source.workspace && workspace.tabs.len() > 1)
            .ok_or(InvalidLayout)?;
        let to = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == destination && workspace.id != source.workspace)
            .ok_or(InvalidLayout)?;
        let index = self.workspaces[from]
            .tabs
            .iter()
            .position(|tab| {
                tab.id == source.tab
                    && matches!(tab.root, Node::Pane { id, .. } if id == source.pane)
            })
            .ok_or(InvalidLayout)?;

        let tab = self.workspaces[from].tabs.remove(index);
        self.workspaces[to].tabs.push(tab);
        Ok(Location {
            workspace: destination,
            ..source
        })
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
                manual_name: Some(label.clone()),
                label,
                root: Node::Pane {
                    id: pane,
                    last_cwd: None,
                    manual_name: None,
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

impl Tab {
    pub fn display_name(&self, index: usize, titles: &HashMap<Uuid, String>) -> String {
        if let Some(name) = &self.manual_name {
            return name.clone();
        }
        if let Node::Pane {
            id, manual_name, ..
        } = &self.root
        {
            return pane_name(*id, manual_name.as_deref(), 0, titles);
        }
        format!("Tab {}", index + 1)
    }
}

pub fn pane_name(
    id: Uuid,
    manual_name: Option<&str>,
    index: usize,
    titles: &HashMap<Uuid, String>,
) -> String {
    manual_name
        .map(str::to_owned)
        .or_else(|| titles.get(&id).filter(|title| !title.is_empty()).cloned())
        .unwrap_or_else(|| format!("Pane {}", index + 1))
}

pub fn clean_title(text: &str) -> String {
    let mut result = String::new();
    for ch in text.chars().filter(|ch| {
        !ch.is_control()
            && !matches!(*ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        if result.len() + ch.len_utf8() > MAX_LABEL {
            break;
        }
        result.push(ch);
    }
    result
}

impl Node {
    fn named_panes<'a>(&'a self, panes: &mut Vec<(Uuid, Option<&'a str>)>) {
        match self {
            Self::Pane {
                id, manual_name, ..
            } => panes.push((*id, manual_name.as_deref())),
            Self::Split { children, .. } => {
                for child in children {
                    child.named_panes(panes);
                }
            }
        }
    }

    fn remove_child_pane(&mut self, pane: Uuid) -> bool {
        let Self::Split { children, .. } = self else {
            return false;
        };
        if let Some(index) = children
            .iter()
            .position(|child| matches!(child, Self::Pane { id, .. } if *id == pane))
        {
            children.remove(index);
        } else if !children
            .iter_mut()
            .any(|child| child.remove_child_pane(pane))
        {
            return false;
        }
        if children.len() == 1 {
            *self = children.pop().expect("one surviving child");
        }
        true
    }

    fn visit_panes(&self, visit: &mut impl FnMut(Uuid)) {
        match self {
            Self::Pane { id, .. } => visit(*id),
            Self::Split { children, .. } => {
                for child in children {
                    child.visit_panes(visit);
                }
            }
        }
    }
    fn contains(&self, pane: Uuid) -> bool {
        match self {
            Self::Pane { id, .. } => *id == pane,
            Self::Split { children, .. } => children.iter().any(|child| child.contains(pane)),
        }
    }

    fn pane_mut(&mut self, pane: Uuid, depth: usize) -> Option<&mut Self> {
        if depth >= MAX_DEPTH {
            return None;
        }
        match self {
            Self::Pane { id, .. } if *id == pane => Some(self),
            Self::Split { children, .. } => children
                .iter_mut()
                .find_map(|child| child.pane_mut(pane, depth + 1)),
            Self::Pane { .. } => None,
        }
    }
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
            Self::Pane { id, last_cwd, .. } if *id == pane => last_cwd.as_ref(),
            Self::Pane { .. } => None,
            Self::Split { children, .. } => children.iter().find_map(|child| child.cwd(pane)),
        }
    }

    fn cwd_mut(&mut self, pane: Uuid) -> Option<&mut Option<CwdObservation>> {
        match self {
            Self::Pane { id, last_cwd, .. } if *id == pane => Some(last_cwd),
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
            Node::Pane {
                id,
                last_cwd,
                manual_name,
            } => {
                if let Some(name) = manual_name {
                    validate_label(name)?;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_titles_follow_panes_and_tab_positions() {
        let mut layout = Layout::empty_terminal();
        let first = layout.primary().unwrap();
        let mut titles = HashMap::from([(first.pane, "한글; build 🐦".to_owned())]);
        assert_eq!(
            layout.display(&titles).workspaces[0].tabs[0].label,
            "한글; build 🐦"
        );
        let split = layout.split_pane(first, Axis::Horizontal).unwrap();
        titles.insert(split.pane, "other".into());
        assert_eq!(layout.display(&titles).workspaces[0].tabs[0].label, "Tab 1");
        assert_eq!(
            layout.pane_title(first, &titles).as_deref(),
            Some("한글; build 🐦")
        );
        let second = layout.add_tab(first.workspace, "temporary".into()).unwrap();
        layout.rename(second, false, None).unwrap();
        layout.split_pane(second, Axis::Vertical).unwrap();
        assert_eq!(layout.display(&titles).workspaces[0].tabs[1].label, "Tab 2");
        layout.close(CloseTarget::Pane(split)).unwrap();
        assert_eq!(
            layout.display(&titles).workspaces[0].tabs[0].label,
            "한글; build 🐦"
        );
        layout
            .close(CloseTarget::Tab {
                workspace: first.workspace,
                tab: first.tab,
            })
            .unwrap();
        assert_eq!(layout.display(&titles).workspaces[0].tabs[0].label, "Tab 1");
    }

    #[test]
    fn manual_names_override_independently_and_reset_to_latest_terminal_title() {
        let mut layout = Layout::empty_terminal();
        let location = layout.primary().unwrap();
        let mut titles = HashMap::from([(location.pane, "first".to_owned())]);
        layout
            .rename(location, true, Some("pane name".into()))
            .unwrap();
        assert_eq!(
            layout.display(&titles).workspaces[0].tabs[0].label,
            "pane name"
        );
        layout
            .rename(location, false, Some("tab name".into()))
            .unwrap();
        titles.insert(location.pane, "latest".into());
        assert_eq!(
            layout.pane_title(location, &titles).as_deref(),
            Some("pane name")
        );
        assert_eq!(
            layout.display(&titles).workspaces[0].tabs[0].label,
            "tab name"
        );
        layout.rename(location, true, None).unwrap();
        assert_eq!(
            layout.pane_title(location, &titles).as_deref(),
            Some("latest")
        );
        assert_eq!(
            layout.display(&titles).workspaces[0].tabs[0].label,
            "tab name"
        );
        layout.rename(location, false, None).unwrap();
        assert_eq!(
            layout.display(&titles).workspaces[0].tabs[0].label,
            "latest"
        );
        titles.clear();
        assert_eq!(
            layout.display(&titles).workspaces[0].tabs[0].label,
            "Pane 1"
        );
    }

    #[test]
    fn terminal_titles_preserve_printable_text_and_bound_utf8() {
        assert_eq!(clean_title(" x;한글🦀 e\u{301} "), " x;한글🦀 e\u{301} ");
        assert_eq!(clean_title("a\n\x1b\u{202e}b"), "ab");
        assert_eq!(clean_title(&"한".repeat(400)).len(), 1023);
    }
}
