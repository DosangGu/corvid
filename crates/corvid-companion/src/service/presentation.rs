use corvid_client::ControlEndpoint;
use corvid_protocol::wire::LocalHostEndpoint;
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::presented_host::Endpoint;
use corvid_protocol::wire::{
    AgentBadge, GetPresentationSlots, HostList, LayoutRecovery, PresentationSlots,
    PresentedBinding, PresentedHost, PresentedRun, PresentedSlot, RunAvailability,
};

use super::{Outcome, Service};
use crate::remote::invalid;
use crate::{MAX_SLOTS, PRESENTATION_PAGE_SIZE, Recovery, RunKey};

impl Service {
    pub(super) fn list_hosts(&self) -> Body {
        let endpoint = match &self.endpoint {
            ControlEndpoint::Local(_) => Endpoint::Local(LocalHostEndpoint {}),
            ControlEndpoint::Ssh(alias) => Endpoint::SshAlias(alias.as_str().to_owned()),
        };
        let remote = self.remote.as_ref().filter(|_| self.generation.is_some());

        Body::HostList(HostList {
            hosts: vec![PresentedHost {
                host_id: self.host.clone(),
                endpoint: Some(endpoint),
                connection: self.connection as i32,
                issue_code: self.issue.map(str::to_owned),
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
        let page_size = PRESENTATION_PAGE_SIZE as usize;
        let offset = (query.overflow_page as usize).saturating_mul(page_size);
        let layout = self.store.layout();
        let slots = layout
            .slots()
            .iter()
            .take(count as usize)
            .enumerate()
            .map(|(index, slot)| PresentedSlot {
                slot_id: index as u32 + 1,
                revision: slot.revision,
                pinned: slot.pinned,
                run: slot.run.map(|run| self.display_run(run)),
            })
            .collect();
        let bindings = self
            .remote
            .as_ref()
            .filter(|_| self.generation.is_some())
            .map_or_else(Vec::new, |remote| {
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
        Ok(Body::PresentationSlots(PresentationSlots {
            controller_id: self.controller.clone(),
            connection: self.connection as i32,
            issue_code: self.issue.map(str::to_owned),
            remote_epoch: self.remote.as_ref().map(|remote| remote.epoch.to_string()),
            remote_sequence: self.remote.as_ref().map(|remote| remote.sequence),
            control_generation: self.generation.map(|generation| generation.to_string()),
            slots,
            overflow: layout
                .overflow()
                .skip(offset)
                .take(page_size)
                .map(|run| self.display_run(run))
                .collect(),
            overflow_count: layout.overflow().count() as u32,
            bindings,
            recent_ended: layout.ended().take(page_size).map(Into::into).collect(),
            suppressed: layout
                .suppressed()
                .skip(offset)
                .take(page_size)
                .map(Into::into)
                .collect(),
            suppressed_count: layout.suppressed().count() as u32,
            layout_recovery: layout_recovery as i32,
        }))
    }

    fn display_run(&self, key: RunKey) -> PresentedRun {
        let mut display = self
            .remote
            .as_ref()
            .and_then(|remote| remote.runs.get(&key))
            .map(|run| run.display.clone())
            .or_else(|| self.store.last_known(key).map(|cached| cached.display(key)))
            .unwrap_or_else(|| PresentedRun {
                key: Some(key.into()),
                last_badge: AgentBadge::Unknown as i32,
                availability: RunAvailability::CheckNeeded as i32,
                ..Default::default()
            });

        if self
            .remote
            .as_ref()
            .is_some_and(|remote| key.epoch != remote.epoch)
        {
            display.availability = RunAvailability::Historical as i32;
        } else if self.generation.is_none() {
            display.availability = RunAvailability::Offline as i32;
        }
        if self.generation.is_none() {
            display.latest_unseen_attention_id = None;
        }
        display
    }
}
