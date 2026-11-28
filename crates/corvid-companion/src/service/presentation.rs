use std::collections::BTreeSet;

use corvid_client::ControlEndpoint;
use corvid_protocol::wire::LocalHostEndpoint;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::presented_host::Endpoint;
use corvid_protocol::wire::{
    AgentBadge, GetPresentationSlots, HostList, LayoutRecovery, PresentationRuns,
    PresentationSlots, PresentedBinding, PresentedHost, PresentedRun, PresentedSlot,
    RunAvailability,
};

use super::{Outcome, Service};
use crate::remote::invalid;
use crate::{MAX_SLOTS, PRESENTATION_PAGE_SIZE, Recovery, RunKey};

impl Service {
    pub(super) fn attention(&self) -> Body {
        let ready = self.connection.ready();
        let remote = ready.map(|snapshot| &snapshot.remote);
        Body::PresentationAttention(corvid_protocol::wire::PresentationAttention {
            controller_id: self.controller.clone(),
            connection: self.connection.status(),
            control_generation: ready.map(|snapshot| snapshot.generation.to_string()),
            remote_epoch: remote.map(|remote| remote.epoch.to_string()),
            remote_sequence: remote.map(|remote| remote.sequence),
            attention: remote.map_or_else(Vec::new, |remote| remote.attention.clone()),
            omitted_count: remote.map_or(0, |remote| remote.omitted_attention),
        })
    }

    pub(super) fn list_hosts(&self) -> Body {
        let endpoint = match &self.endpoint {
            ControlEndpoint::Local(_) => Endpoint::Local(LocalHostEndpoint {}),
            ControlEndpoint::Ssh(alias) => Endpoint::SshAlias(alias.as_str().to_owned()),
            ControlEndpoint::Wsl(distribution) => {
                Endpoint::WslDistribution(distribution.as_str().to_owned())
            }
        };
        let remote = self.connection.ready().map(|snapshot| &snapshot.remote);

        Body::HostList(HostList {
            hosts: vec![PresentedHost {
                host_id: self.host.clone(),
                endpoint: Some(endpoint),
                connection: self.connection.status(),
                issue_code: self.connection.issue().map(str::to_owned),
                remote_epoch: remote.map(|remote| remote.epoch.to_string()),
                remote_sequence: remote.map(|remote| remote.sequence),
            }],
        })
    }

    pub(super) fn presentation(&self, query: GetPresentationSlots) -> Outcome {
        let count = query.count.unwrap_or(self.store.config().slot_count);
        if !(1..=MAX_SLOTS).contains(&count) {
            return Err(invalid());
        }
        Ok(Body::PresentationSlots(self.present_slots(
            count,
            query.overflow_page,
            None,
        )))
    }

    pub(super) fn current_runs(&self, page: u32) -> Body {
        let mut current = BTreeSet::new();
        if let Some(snapshot) = self.connection.ready() {
            current.extend(
                snapshot
                    .remote
                    .runs
                    .iter()
                    .filter_map(|(&key, run)| run.observation.eligible.then_some(key)),
            );
        }
        Body::PresentationRuns(PresentationRuns {
            page: Some(self.present_slots(self.store.config().slot_count, page, Some(current))),
            layout_revision: self.store.layout_revision(),
            state_revision: self.state_revision(),
        })
    }

    fn present_slots(
        &self,
        count: u32,
        page: u32,
        mut current: Option<BTreeSet<RunKey>>,
    ) -> PresentationSlots {
        let page_size = PRESENTATION_PAGE_SIZE as usize;
        let offset = (page as usize).saturating_mul(page_size);
        let layout = self.store.layout();
        let suppressed: Vec<_> = layout
            .suppressed()
            .filter(|key| current.as_mut().is_none_or(|runs| runs.remove(key)))
            .collect();
        let slots = layout
            .slots()
            .iter()
            .take(count as usize)
            .enumerate()
            .map(|(index, slot)| PresentedSlot {
                slot_id: index as u32 + 1,
                revision: slot.revision,
                pinned: slot.pinned,
                run: slot
                    .run
                    .filter(|key| current.as_mut().is_none_or(|runs| runs.remove(key)))
                    .map(|run| self.display_run(run)),
            })
            .collect();
        let mut overflow: Vec<_> = layout
            .overflow()
            .filter(|key| current.as_mut().is_none_or(|runs| runs.remove(key)))
            .collect();
        if let Some(unassigned) = current {
            let remote = self.connection.ready().map(|snapshot| &snapshot.remote);
            let mut remaining: Vec<_> = unassigned.into_iter().collect();
            remaining.sort_by_key(|key| {
                let sequence = remote
                    .and_then(|remote| remote.runs.get(key))
                    .and_then(|run| run.observation.created_sequence);
                (sequence.unwrap_or(u64::MAX), *key)
            });
            overflow.extend(remaining);
        }
        let overflow_count = overflow.len() as u32;
        let ready = self.connection.ready();
        let latest = self.latest_snapshot();
        let bindings = ready.map_or_else(Vec::new, |snapshot| {
            let remote = &snapshot.remote;
            remote
                .bindings
                .iter()
                .map(|(client, revision)| PresentedBinding {
                    client_id: client.to_string(),
                    revision: *revision,
                    #[cfg(windows)]
                    wezterm: self
                        .local_registration(remote, *client, *revision)
                        .and_then(|registration| registration.pane.as_ref())
                        .map(|pane| pane.location()),
                    #[cfg(not(windows))]
                    wezterm: None,
                })
                .collect()
        });
        let layout_recovery = match self.store.recovery() {
            Recovery::Normal => LayoutRecovery::Normal,
            Recovery::Backup => LayoutRecovery::Backup,
            Recovery::Fresh => LayoutRecovery::Fresh,
        };
        PresentationSlots {
            controller_id: self.controller.clone(),
            connection: self.connection.status(),
            issue_code: self.connection.issue().map(str::to_owned),
            remote_epoch: latest.map(|remote| remote.epoch.to_string()),
            remote_sequence: latest.map(|remote| remote.sequence),
            control_generation: ready.map(|snapshot| snapshot.generation.to_string()),
            slots,
            overflow: overflow
                .into_iter()
                .skip(offset)
                .take(page_size)
                .map(|run| self.display_run(run))
                .collect(),
            overflow_count,
            bindings,
            recent_ended: layout.ended().take(page_size).map(Into::into).collect(),
            suppressed_count: suppressed.len() as u32,
            suppressed: suppressed
                .into_iter()
                .skip(offset)
                .take(page_size)
                .map(Into::into)
                .collect(),
            layout_recovery: layout_recovery as i32,
        }
    }

    fn display_run(&self, key: RunKey) -> PresentedRun {
        let latest = self.latest_snapshot();
        let ready = self.connection.ready().is_some();
        let mut display = latest
            .and_then(|remote| remote.runs.get(&key))
            .map(|run| run.display.clone())
            .or_else(|| self.store.last_known(key).map(|cached| cached.display(key)))
            .unwrap_or_else(|| PresentedRun {
                key: Some(key.into()),
                last_badge: AgentBadge::Unknown as i32,
                availability: RunAvailability::CheckNeeded as i32,
                ..Default::default()
            });

        if latest.is_some_and(|remote| key.epoch != remote.epoch) {
            display.availability = RunAvailability::Historical as i32;
        } else if !ready {
            display.availability = RunAvailability::Offline as i32;
        }
        if !ready {
            display.latest_unseen_attention_id = None;
            display.program_status = None;
        }
        display
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use anyhow::Result;
    use uuid::Uuid;

    use super::super::Connection;
    use super::*;
    use crate::remote::ObservedRun;
    use crate::{HostEndpoint, RunObservation, Store, initialize};

    #[test]
    fn current_pages_follow_the_daemon_and_apply_saved_preferences() -> Result<()> {
        #[cfg(unix)]
        let root = tempfile::tempdir_in(std::env::home_dir().unwrap())?;
        #[cfg(not(unix))]
        let root = tempfile::tempdir()?;
        let state = root.path().join("companion");
        let endpoint = ControlEndpoint::Local(root.path().join("daemon"));
        let host = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let key = |id: u128| RunKey {
            host,
            epoch,
            run: Uuid::from_u128(id),
        };
        let observation = RunObservation {
            created_sequence: Some(1),
            pane: Some(Uuid::new_v4()),
            navigable: true,
            eligible: true,
            ended: false,
        };
        initialize(&state, host, HostEndpoint::from(&endpoint), 2)?;
        let mut store = Store::open(&state)?;
        let (missing, pinned, hidden) = (key(1), key(2), key(3));
        store.update(|layout| {
            let saved = [missing, pinned, hidden].map(|key| (key, observation));
            layout.reconcile(&BTreeMap::from(saved), &BTreeSet::new())?;
            layout.pin(2, layout.slots()[1].revision, Some(pinned), true)?;
            layout.remove(hidden)?;
            Ok(())
        })?;
        let (changes, _) = corvid_async::watch::channel(1u64);
        let mut service = Service::new(store, endpoint, changes);
        let current = |key: RunKey, eligible| {
            ObservedRun::for_test(
                RunObservation {
                    eligible,
                    ..observation
                },
                PresentedRun {
                    key: Some(key.into()),
                    availability: RunAvailability::Available as i32,
                    ..Default::default()
                },
            )
        };
        let count = u128::from(PRESENTATION_PAGE_SIZE);
        let mut runs = BTreeMap::from([
            (pinned, current(pinned, true)),
            (hidden, current(hidden, true)),
            (key(4), current(key(4), false)),
        ]);
        for id in 5..=count + 5 {
            runs.insert(key(id), current(key(id), true));
        }
        service.publish_ready(crate::remote::Remote {
            epoch,
            sequence: 42,
            bindings: BTreeMap::new(),
            runs,
            panes: BTreeSet::new(),
            attention: Vec::new(),
            omitted_attention: 0,
        });

        let read = |service: &Service, index| {
            let Body::PresentationRuns(page) = service.current_runs(index) else {
                unreachable!()
            };
            (page.layout_revision, page.page.unwrap())
        };
        let (revision, first) = read(&service, 0);
        let (second_revision, second) = read(&service, 1);
        assert_eq!(revision, second_revision);
        assert_eq!(first.remote_sequence, Some(42));
        assert!(first.slots[0].run.is_none());
        assert!(first.slots[1].pinned);
        assert_eq!(
            first.slots[1].run.as_ref().unwrap().key,
            Some(pinned.into())
        );
        assert_eq!(first.suppressed, [hidden.into()]);
        assert_eq!(first.overflow_count, PRESENTATION_PAGE_SIZE + 1);
        assert_eq!(first.overflow.len(), PRESENTATION_PAGE_SIZE as usize);
        assert_eq!(second.overflow.len(), 1);
        assert_eq!(second.overflow[0].key, Some(key(count + 5).into()));

        service.store.update(|layout| {
            layout.pin(2, first.slots[1].revision, Some(pinned), false)?;
            Ok(())
        })?;
        assert_ne!(read(&service, 0).0, revision);
        service.retire_snapshot(Connection::Offline("ConnectionRequired"));
        let (_, offline) = read(&service, 0);
        assert!(offline.slots.iter().all(|slot| slot.run.is_none()));
        assert_eq!(offline.overflow_count, 0);
        assert_eq!(offline.suppressed_count, 0);
        let Body::PresentationSlots(legacy) =
            service.presentation(GetPresentationSlots::default())?
        else {
            unreachable!()
        };
        assert_eq!(
            legacy.slots[0].run.as_ref().unwrap().key,
            Some(missing.into())
        );
        Ok(())
    }
}
