use std::collections::HashSet;
use std::sync::Arc;

use corvid_core::layout::{CloseTarget, Layout, Location};
use uuid::Uuid;

use super::{
    Active, Registry, Reply, ReservedSwitch, Switched, Workspaces, input_busy, open_pane,
    target_gone, writer_required,
};
use crate::publication::Group;
#[cfg(target_os = "linux")]
use corvid_core::agent::ProcessLiveness;

pub(crate) enum Closed {
    Unchanged,
    Switched(Switched),
    Detached,
}

struct Closing<'a>(&'a Workspaces, CloseTarget);

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        self.0
            .lock_registry()
            .closing
            .retain(|target| *target != self.1);
    }
}

impl Workspaces {
    pub(crate) async fn close(&self, old: &Active, target: CloseTarget) -> Reply<Closed> {
        let client = old.route.client_id;
        let (reserved, _closing) = {
            let mut registry = self.lock_registry();
            registry.require_route(old.route)?;
            registry.require_pane_writer(client, old.location)?;
            let shared = registry.clients[&client].keymap.pane_writer;
            if shared && target.workspace() != old.location.workspace {
                return Err(writer_required());
            }
            if !shared {
                registry.require_writer(client, target.workspace())?;
            }
            let mut layout = self.metadata.layout();
            let removed = Arc::make_mut(&mut layout)
                .close(target)
                .map_err(|_| target_gone())?;
            for location in &removed {
                registry.check_pane_available(client, *location)?;
                if registry
                    .slots
                    .get(&location.pane)
                    .is_some_and(|slot| slot.reserved)
                {
                    return Err(input_busy());
                }
            }
            if !target.contains(old.location) {
                registry.commit_close(layout, &removed, target, client, false, None);
                return Ok(Closed::Unchanged);
            }
            let reserved = registry
                .close_destination(&layout, old)
                .map(|location| {
                    let generation = if shared {
                        old.route.writer_lease_generation
                    } else {
                        registry.require_writer(client, location.workspace)?
                    };
                    self.reserve_view(&mut registry, old, location, generation, None)
                })
                .transpose()?;
            registry.closing.push(target);
            (reserved, Closing(self, target))
        };

        let destination = reserved.as_ref().map(|reserved| reserved.location.pane);
        let result = async {
            let next = match reserved {
                Some(reserved) => Some(self.open_close_destination(reserved).await?),
                None => None,
            };
            let paste_discarded = old.terminal.fence(old.route).await?;
            Ok::<_, corvid_protocol::ProtocolFault>((next, paste_discarded))
        }
        .await;

        let mut registry = self.lock_registry();
        if let Some(pane) = destination {
            if let Some(slot) = registry.slots.get_mut(&pane) {
                slot.reserved = false;
            }
            if result.is_err() {
                registry.remove_pane_claim(pane, client);
            }
        }
        let (next, paste_discarded) = result?;
        // Apply to the latest layout so disjoint edits made during fencing survive.
        let mut layout = self.metadata.layout();
        let removed = Arc::make_mut(&mut layout)
            .close(target)
            .map_err(|_| target_gone())?;
        registry.commit_close(layout, &removed, target, client, true, next.as_ref());
        Ok(match next {
            Some(active) => Closed::Switched(Switched {
                active,
                paste_discarded,
            }),
            None => Closed::Detached,
        })
    }

    async fn open_close_destination(&self, reserved: ReservedSwitch) -> Reply<Active> {
        let ReservedSwitch {
            location,
            terminal,
            spec,
            events,
        } = reserved;
        let active = open_pane(terminal, location, spec, events).await?;
        active
            .terminal
            .activate(active.route, active.viewport)
            .await?;
        Ok(active)
    }
}

impl Registry {
    fn close_destination(&self, layout: &Layout, old: &Active) -> Option<Location> {
        let here = old.location;
        let client = old.route.client_id;
        if self.clients[&client].keymap.pane_writer {
            let workspace = layout
                .workspaces
                .iter()
                .find(|item| item.id == here.workspace)?;
            return layout
                .panes(here.workspace, here.tab)
                .into_iter()
                .chain(
                    workspace
                        .tabs
                        .iter()
                        .filter(|tab| tab.id != here.tab)
                        .flat_map(|tab| layout.panes(here.workspace, tab.id)),
                )
                .find(|location| {
                    self.has_live_at(*location)
                        && self.check_pane_available(client, *location).is_ok()
                });
        }
        let current = layout
            .workspaces
            .iter()
            .find(|workspace| workspace.id == here.workspace);
        let other = layout
            .workspaces
            .iter()
            .filter(|workspace| workspace.id != here.workspace);
        self.live_tab(layout, here.workspace, here.tab).or_else(|| {
            current
                .into_iter()
                .chain(other)
                .filter(|workspace| {
                    self.require_writer(old.route.client_id, workspace.id)
                        .is_ok()
                })
                .flat_map(|workspace| workspace.tabs.iter().map(move |tab| (workspace.id, tab.id)))
                .find_map(|(workspace, tab)| self.live_tab(layout, workspace, tab))
        })
    }

    fn commit_close(
        &mut self,
        layout: Arc<Layout>,
        removed: &[Location],
        target: CloseTarget,
        client: Uuid,
        moved: bool,
        next: Option<&Active>,
    ) {
        let panes: HashSet<_> = removed.iter().map(|location| location.pane).collect();
        for pane in &panes {
            self.pane_claims.remove(pane);
            if let Some(slot) = self.slots.remove(pane) {
                self.retired.push(slot.task);
            }
        }

        let mut projections = Vec::new();
        self.terminals.retain(|id, location| {
            if !panes.contains(&location.pane) {
                return true;
            }
            #[cfg(target_os = "linux")]
            {
                if let Some(agent) = self.shells.remove(*id) {
                    self.agents.retire_shell(agent, ProcessLiveness::Unknown);
                }
            }
            self.agents.terminal_unavailable(*id);
            projections.push((Group::Terminal(*id), Vec::new()));
            false
        });
        if layout.locate(Some(target.workspace()), None).is_none() {
            self.claims.remove(&target.workspace());
        }
        if let Some(next) = next {
            self.set_view(next);
        } else if moved {
            self.clients
                .get_mut(&client)
                .expect("closing client still exists")
                .view = None;
        }
        projections.push(self.client_projection(client));
        self.publish_layout(layout, projections);
    }
}
