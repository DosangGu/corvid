use std::collections::HashMap;

use corvid_core::layout::{Axis, Layout, Location, Node, Tab};
use uuid::Uuid;

const MAX_TABS: usize = 8;
const MAX_TAB_PANES: usize = 4;
pub const MAX_LOCAL_PANES: usize = 16;

pub struct SessionLayout {
    layout: Layout,
    active: Location,
    focus: HashMap<Uuid, Uuid>,
    next_tab: usize,
}

impl Default for SessionLayout {
    fn default() -> Self {
        let mut layout = Layout::empty_terminal();
        let active = layout.primary().expect("a new session contains one pane");
        layout.workspaces[0].tabs[0].label = "Shell 1".to_owned();
        Self {
            layout,
            active,
            focus: HashMap::from([(active.tab, active.pane)]),
            next_tab: 2,
        }
    }
}

impl SessionLayout {
    pub fn active(&self) -> Location {
        self.active
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.layout.workspaces[0].tabs
    }

    pub fn active_tab(&self) -> &Tab {
        self.tabs()
            .iter()
            .find(|tab| tab.id == self.active.tab)
            .expect("active tab exists")
    }

    pub fn can_add_tab(&self) -> bool {
        self.tabs().len() < MAX_TABS && self.has_room()
    }

    pub fn can_split(&self) -> bool {
        pane_count(&self.active_tab().root) < MAX_TAB_PANES && self.has_room()
    }

    fn has_room(&self) -> bool {
        self.tabs()
            .iter()
            .map(|tab| pane_count(&tab.root))
            .sum::<usize>()
            < MAX_LOCAL_PANES
    }

    pub fn can_close_pane(&self) -> bool {
        self.tabs().len() > 1 || pane_count(&self.active_tab().root) > 1
    }

    pub fn add_tab(&mut self) -> Option<Uuid> {
        if !self.can_add_tab() {
            return None;
        }
        let label = format!("Shell {}", self.next_tab);
        let location = self.layout.add_tab(self.active.workspace, label).ok()?;
        self.next_tab += 1;
        self.active = location;
        self.focus.insert(location.tab, location.pane);
        Some(location.pane)
    }

    pub fn select_tab(&mut self, tab: Uuid) -> bool {
        let Some(pane) = self.focus.get(&tab).copied() else {
            return false;
        };
        self.active.tab = tab;
        self.active.pane = pane;
        true
    }

    pub fn cycle_tab(&mut self, forward: bool) {
        let tabs = self.tabs();
        let current = tabs
            .iter()
            .position(|tab| tab.id == self.active.tab)
            .expect("active tab exists");
        let offset = if forward { 1 } else { tabs.len() - 1 };
        self.select_tab(tabs[(current + offset) % tabs.len()].id);
    }

    pub fn select_pane(&mut self, pane: Uuid) -> bool {
        if !contains(&self.active_tab().root, pane) {
            return false;
        }
        self.active.pane = pane;
        self.focus.insert(self.active.tab, pane);
        true
    }

    pub fn split(&mut self, axis: Axis) -> Option<Uuid> {
        if !self.can_split() {
            return None;
        }
        let original = self.active.pane;
        let new = Uuid::new_v4();
        let tab = self.layout.workspaces[0]
            .tabs
            .iter_mut()
            .find(|tab| tab.id == self.active.tab)?;
        let target = pane_mut(&mut tab.root, original)?;
        let right = Node::Pane {
            id: new,
            last_cwd: None,
        };
        let left = std::mem::replace(
            target,
            Node::Split {
                axis,
                children: Vec::with_capacity(2),
            },
        );
        let Node::Split { children, .. } = target else {
            unreachable!()
        };
        children.extend([left, right]);
        self.select_pane(new);
        Some(new)
    }

    pub fn close_pane(&mut self, pane: Uuid) -> bool {
        if !self.can_close_pane() || !contains(&self.active_tab().root, pane) {
            return false;
        }
        if matches!(self.active_tab().root, Node::Pane { .. }) {
            self.close_tab(self.active.tab);
            return true;
        }
        let tab = self.layout.workspaces[0]
            .tabs
            .iter_mut()
            .find(|tab| tab.id == self.active.tab)
            .expect("active tab exists");
        remove(&mut tab.root, pane);
        if pane == self.active.pane {
            let neighbor = first_pane(&tab.root);
            self.select_pane(neighbor);
        }
        true
    }

    pub fn close_tab(&mut self, id: Uuid) -> Vec<Uuid> {
        let tabs = &mut self.layout.workspaces[0].tabs;
        if tabs.len() == 1 {
            return Vec::new();
        }
        let Some(index) = tabs.iter().position(|tab| tab.id == id) else {
            return Vec::new();
        };
        let removed = tabs.remove(index);
        let next = tabs[index.min(tabs.len() - 1)].id;
        self.focus.remove(&id);
        if self.active.tab == id {
            self.select_tab(next);
        }
        let mut panes = Vec::new();
        collect_panes(&removed.root, &mut panes);
        panes
    }
}

fn pane_count(node: &Node) -> usize {
    match node {
        Node::Pane { .. } => 1,
        Node::Split { children, .. } => children.iter().map(pane_count).sum(),
    }
}

fn contains(node: &Node, pane: Uuid) -> bool {
    match node {
        Node::Pane { id, .. } => *id == pane,
        Node::Split { children, .. } => children.iter().any(|child| contains(child, pane)),
    }
}

fn first_pane(node: &Node) -> Uuid {
    match node {
        Node::Pane { id, .. } => *id,
        Node::Split { children, .. } => first_pane(&children[0]),
    }
}

fn collect_panes(node: &Node, output: &mut Vec<Uuid>) {
    match node {
        Node::Pane { id, .. } => output.push(*id),
        Node::Split { children, .. } => {
            for child in children {
                collect_panes(child, output);
            }
        }
    }
}

fn pane_mut(node: &mut Node, pane: Uuid) -> Option<&mut Node> {
    match node {
        Node::Pane { id, .. } if *id == pane => Some(node),
        Node::Pane { .. } => None,
        Node::Split { children, .. } => children.iter_mut().find_map(|child| pane_mut(child, pane)),
    }
}

fn remove(node: &mut Node, pane: Uuid) {
    let Node::Split { children, .. } = node else {
        return;
    };
    if let Some(index) = children
        .iter()
        .position(|child| matches!(child, Node::Pane { id, .. } if *id == pane))
    {
        children.remove(index);
        if children.len() == 1 {
            *node = children.remove(0);
        }
    } else if let Some(child) = children.iter_mut().find(|child| contains(child, pane)) {
        remove(child, pane);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_focus_per_tab_and_collapses_nested_splits() {
        let mut session = SessionLayout::default();
        let first = session.active();
        let right = session.split(Axis::Horizontal).unwrap();
        let lower = session.split(Axis::Vertical).unwrap();
        assert_eq!(session.active().pane, lower);
        assert!(session.layout.validate().is_ok());

        let second = session.add_tab().unwrap();
        assert_ne!(session.active().tab, first.tab);
        assert!(!session.select_pane(lower));
        session.cycle_tab(false);
        assert_eq!(session.active().pane, lower);
        assert!(session.close_pane(lower));
        assert!(contains(&session.active_tab().root, right));
        assert!(session.close_pane(right));
        assert!(matches!(session.active_tab().root, Node::Pane { id, .. } if id == first.pane));
        assert!(session.layout.validate().is_ok());

        assert!(session.close_pane(first.pane));
        assert_eq!(session.active().pane, second);
        assert!(!session.close_pane(second));
    }

    #[test]
    fn closing_an_inactive_tab_returns_only_its_shells() {
        let mut session = SessionLayout::default();
        let first = session.active();
        let split = session.split(Axis::Horizontal).unwrap();
        let active = session.add_tab().unwrap();
        assert_eq!(session.close_tab(first.tab), [first.pane, split]);
        assert_eq!(session.active().pane, active);
        assert!(session.layout.validate().is_ok());
    }

    #[test]
    fn caps_panes_per_tab() {
        let mut session = SessionLayout::default();
        for _ in 1..MAX_TAB_PANES {
            assert!(session.split(Axis::Horizontal).is_some());
        }
        assert!(session.split(Axis::Vertical).is_none());
        assert!(session.add_tab().is_some());
    }
}
