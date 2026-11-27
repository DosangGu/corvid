use std::collections::HashSet;
use std::sync::Arc;

use corvid_core::layout::{CloseTarget, Layout, Location};
use uuid::Uuid;

use super::{
    Active, Registry, Reply, ReservedSwitch, Switched, Workspaces, input_busy, open_pane,
    target_gone, writer_required,
};
use crate::publication::Group;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_core::agent::ProcessLiveness;

pub(crate) enum Closed {
    Unchanged,
    Switched(Switched),
    Detached,
}

struct Closing(super::RegistryActor, CloseTarget);

impl Drop for Closing {
    fn drop(&mut self) {
        let target = self.1;
        self.0
            .defer(move |registry| registry.closing.retain(|pending| *pending != target));
    }
}

impl Workspaces {
    pub(crate) async fn close(&self, old: &Active, target: CloseTarget) -> Reply<Closed> {
        let client = old.route.client_id;
        let source = old.view();
        let reservation = self
            .transaction(move |registry, workspaces| {
                let old = &source;
                registry.require_route(old.route)?;
                registry.require_pane_writer(client, old.location)?;
                let shared = registry.clients[&client].keymap.pane_writer;
                if shared && target.workspace() != old.location.workspace {
                    return Err(writer_required());
                }
                if !shared {
                    registry.require_writer(client, target.workspace())?;
                }
                let previous = registry.metadata.layout();
                let mut layout = Arc::clone(&previous);
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
                    return Ok(None);
                }
                let reserved = registry
                    .close_destination(&previous, &layout, old)
                    .map(|location| {
                        let generation = if shared {
                            old.route.writer_lease_generation
                        } else {
                            registry.require_writer(client, location.workspace)?
                        };
                        workspaces.reserve_view(registry, old, location, generation, None, false)
                    })
                    .transpose()?;
                registry.closing.push(target);
                Ok(Some((
                    reserved,
                    Closing(workspaces.registry.clone(), target),
                )))
            })
            .await?;
        let Some((reserved, _closing)) = reservation else {
            return Ok(Closed::Unchanged);
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

        self.registry
            .request(move |registry| {
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
                let mut layout = registry.metadata.layout();
                let removed = Arc::make_mut(&mut layout)
                    .close(target)
                    .map_err(|_| target_gone())?;
                registry.commit_close(
                    layout,
                    &removed,
                    target,
                    client,
                    true,
                    next.as_ref().map(Active::view).as_ref(),
                );
                Ok(match next {
                    Some(active) => Closed::Switched(Switched {
                        active,
                        paste_discarded,
                    }),
                    None => Closed::Detached,
                })
            })
            .await
    }

    async fn open_close_destination(&self, reserved: ReservedSwitch) -> Reply<Active> {
        let ReservedSwitch {
            location,
            terminal,
            spec,
            events,
            created_layout: _,
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
    fn close_destination(
        &self,
        previous: &Layout,
        layout: &Layout,
        old: &super::View,
    ) -> Option<Location> {
        let here = old.location;
        let client = old.route.client_id;
        let available_pane = |workspace, tab| {
            layout.panes(workspace, tab).into_iter().find(|location| {
                self.slots
                    .get(&location.pane)
                    .is_some_and(|slot| !slot.reserved && slot.terminal.has_live_terminal())
                    && self.check_pane_available(client, *location).is_ok()
            })
        };
        if let Some(location) = available_pane(here.workspace, here.tab) {
            return Some(location);
        }
        let workspace = previous
            .workspaces
            .iter()
            .find(|workspace| workspace.id == here.workspace)?;
        let index = workspace.tabs.iter().position(|tab| tab.id == here.tab)?;
        let neighbor = (1..workspace.tabs.len())
            .flat_map(|distance| [Some(index + distance), index.checked_sub(distance)])
            .flatten()
            .filter_map(|index| workspace.tabs.get(index))
            .find_map(|tab| available_pane(here.workspace, tab.id));
        if neighbor.is_some() || self.clients[&client].keymap.pane_writer {
            return neighbor;
        }
        layout
            .workspaces
            .iter()
            .filter(|workspace| {
                workspace.id != here.workspace && self.require_writer(client, workspace.id).is_ok()
            })
            .find_map(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .find_map(|tab| available_pane(workspace.id, tab.id))
            })
    }

    fn commit_close(
        &mut self,
        layout: Arc<Layout>,
        removed: &[Location],
        target: CloseTarget,
        client: Uuid,
        moved: bool,
        next: Option<&super::View>,
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
            #[cfg(any(target_os = "linux", target_os = "macos"))]
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
