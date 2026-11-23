use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use corvid_core::agent::{
    Collection, Confidence, Lifecycle, ObservationKind, ProcessLiveness, Provider, Source,
};
use corvid_core::layout::Location;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Agents, evidence};
use crate::handoff::input::{RetainedInput, valid_id};
use crate::process::{HookProducer, ProcessImage};

/// Private handoff metadata, never terminal content or a live controller address.
#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct RetainedAgent {
    pub(crate) association: Uuid,
    pub(crate) observer_epoch: Uuid,
    pub(crate) owner: ProcessImage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restart_path: Option<Vec<u8>>,
    provider: Provider,
    producer: Option<HookProducer>,
    session: String,
    sessions: Vec<String>,
    turn: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<RetainedInput>,
    gap: bool,
}

impl RetainedAgent {
    #[cfg(target_os = "linux")]
    pub(crate) fn legacy(
        root: crate::process::Incarnation,
        session: corvid_core::agent::AgentSessionKey,
        host: Uuid,
    ) -> Option<Self> {
        let (owner, association, observer_epoch) =
            crate::process::legacy_observer(root, session.provider, host)?;
        Some(Self {
            association,
            observer_epoch,
            owner,
            restart_path: None,
            provider: session.provider,
            producer: None,
            session: session.provider_session_id.clone(),
            sessions: vec![session.provider_session_id],
            turn: None,
            inputs: Vec::new(),
            gap: false,
        })
    }

    pub(crate) fn valid(&self) -> bool {
        !self.association.is_nil()
            && !self.observer_epoch.is_nil()
            && valid_id(&self.session)
            && self.sessions.len() <= 256
            && self.sessions.iter().all(|id| valid_id(id))
            && self.sessions.contains(&self.session)
            && self.turn.as_deref().is_none_or(valid_id)
            && RetainedInput::valid_all(self.turn.as_deref(), &self.inputs)
            && self.restart_path.as_ref().is_none_or(|path| {
                self.provider == Provider::Claude
                    && path.len() <= 4096
                    && !path.contains(&0)
                    && Path::new(OsStr::from_bytes(path)).is_absolute()
            })
    }

    pub(crate) fn restart_path(&self) -> Option<Arc<Path>> {
        self.restart_path
            .as_deref()
            .map(|path| Arc::from(Path::new(OsStr::from_bytes(path))))
    }

    pub(crate) fn recover_restart_path(&mut self) {
        if self.provider == Provider::Claude && self.restart_path.is_none() {
            self.restart_path = self
                .owner
                .recover_launcher(self.provider)
                .map(|path| path.as_os_str().as_bytes().to_vec())
                .filter(|path| path.len() <= 4096);
        }
    }
}

impl Agents {
    pub(crate) fn retain(&self, terminal: Uuid, epoch: Uuid) -> Option<RetainedAgent> {
        self.launches.iter().find_map(|(association, launch)| {
            let record = self.runs.get(&launch.current)?;
            if record.run.identity.terminal_id != terminal
                || !record.published
                || !launch.qualified
                || launch.ambiguous
                || launch.resume.is_some()
                || record.run.lifecycle.value != Lifecycle::Live
                || record.run.process.value != ProcessLiveness::Running
            {
                return None;
            }
            Some(RetainedAgent {
                association: *association,
                observer_epoch: launch.observer_epoch.unwrap_or(epoch),
                owner: launch.owner?,
                restart_path: launch
                    .restart_path
                    .as_deref()
                    .map(|path| path.as_os_str().as_bytes().to_vec()),
                provider: record.run.identity.provider,
                producer: launch.producer,
                session: record.run.identity.provider_session.clone()?,
                sessions: launch.sessions.iter().cloned().collect(),
                turn: record.run.current_open_turn().map(str::to_owned),
                inputs: RetainedInput::collect(&record.run),
                gap: launch.gap,
            })
        })
    }

    pub(crate) fn accepts_observer_epoch(
        &self,
        association: Uuid,
        epoch: Uuid,
        current: Uuid,
    ) -> bool {
        self.launches
            .get(&association)
            .is_some_and(|launch| epoch == launch.observer_epoch.unwrap_or(current))
    }

    pub(crate) fn recover(
        &mut self,
        terminal: Uuid,
        location: Location,
        saved: RetainedAgent,
    ) -> bool {
        if !saved.valid() || self.launches.contains_key(&saved.association) {
            return false;
        }
        let temporary = self.reserve(location, terminal, saved.provider, true);
        let mut launch = self.launches.remove(&temporary).expect("reserved launch");
        let record = self.runs.get_mut(&launch.current).expect("reserved run");
        record.run.identity.launch_association = saved.association;
        record.observe_process(
            ProcessLiveness::Running,
            Some(saved.owner.incarnation),
            Source::ProcessProbe,
        );
        record.published = true;
        record.terminal_available = true;
        launch.owner = Some(saved.owner);
        launch.restart_path = saved.restart_path();
        launch.producer = saved.producer;
        launch.observer_epoch = Some(saved.observer_epoch);
        launch.retained = true;
        launch.sessions = saved.sessions.into_iter().collect();
        launch.gap = saved.gap;
        launch.establish_context(record, Uuid::new_v4(), &saved.session);
        if let Some(turn) = saved.turn {
            record.apply(
                Uuid::new_v4(),
                ObservationKind::InteractivePromptSubmitted(turn.clone()),
                evidence(Source::Collector, Confidence::Confirmed),
            );
            for input in saved.inputs {
                input.restore(
                    &mut record.run,
                    &turn,
                    &evidence(Source::Collector, Confidence::Confirmed),
                );
            }
        }
        // Keep historical attention out of the new run. A fresh authenticated
        // observation must confirm collection in this epoch before notifying.
        record.apply(
            Uuid::new_v4(),
            ObservationKind::Collection(Collection::Degraded),
            evidence(Source::Collector, Confidence::Confirmed),
        );
        self.dirty.insert(launch.current);
        self.launches.insert(saved.association, launch);
        true
    }
}
