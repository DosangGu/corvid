use super::*;
use crate::inventory_runtime;

/// Keep metadata and its SSH connection independent of ANSI parsing and writer I/O.
/// Joining on drop fences publication before another attachment can use Shared.
pub(super) struct InventoryWatch {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
    pub(super) finished: oneshot::Receiver<()>,
}

impl InventoryWatch {
    pub(super) fn start(
        endpoint: ControlEndpoint,
        host: String,
        epoch: String,
        shared: Arc<Shared>,
    ) -> Result<Self> {
        let subscription = crate::snapshot_hub::subscribe(endpoint, host)?;
        let (stop, stopped) = oneshot::channel();
        let (finished, receive_finished) = oneshot::channel();
        let thread = inventory_runtime::start("corvid-desktop-inventory", move || async move {
            corvid_async::select! {
                _ = stopped => {}
                _ = forward(subscription, &epoch, &shared) => {}
            }
            let _ = finished.send(());
        })?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
            finished: receive_finished,
        })
    }
}

impl Drop for InventoryWatch {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn forward(subscription: crate::snapshot_hub::Subscription, epoch: &str, shared: &Shared) {
    let mut updates = subscription.receiver();
    let mut manual = shared.ssh_control_reconnect.subscribe();
    apply_update(updates.borrow().clone(), epoch, shared);
    loop {
        corvid_async::select! {
            Ok(()) = manual.changed() => subscription.reconnect(),
            changed = updates.changed() => {
                if changed.is_err() {
                    return;
                }
                apply_update(updates.borrow().clone(), epoch, shared);
            }
        }
    }
}

fn apply_update(update: crate::snapshot_hub::SnapshotUpdate, epoch: &str, shared: &Shared) {
    if update.epoch.as_deref() == Some(epoch)
        && let Some(inventory) = update.inventory
    {
        publish(shared, (*inventory).clone());
    } else {
        let offline = update.offline;
        shared.update_ui(move |ui| {
            ui.inventory_ready = false;
            ui.inventory_offline = offline;
        });
    }
}

fn publish(shared: &Shared, inventory: Inventory) {
    shared.edit(move |state| {
        let active_pane = state
            .ui
            .selected
            .as_ref()
            .map(|selected| selected.pane_id.clone());
        let screens = Arc::make_mut(&mut state.pane_screens);
        screens.retain(|pane, _| {
            active_pane.as_deref() == Some(pane.as_str())
                || inventory.terminals.iter().any(|terminal| {
                    terminal
                        .location
                        .as_ref()
                        .is_some_and(|location| &location.pane_id == pane)
                })
        });
        state.pane_order.retain(|pane| screens.contains_key(pane));
        let ui = Arc::make_mut(&mut state.ui);
        ui.inventory = Some(Arc::new(inventory));
        ui.inventory_ready = true;
        ui.inventory_offline = false;
        Changed::Ui
    });
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod terminal_tests;
