use super::*;
use crate::inventory_runtime::{self, sleep, sleep_until, timeout_at};

const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(3);

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
        let (stop, stopped) = oneshot::channel();
        let (finished, receive_finished) = oneshot::channel();
        let thread = inventory_runtime::start("corvid-desktop-inventory", move || async move {
            corvid_async::select! {
                _ = stopped => {}
                _ = monitor(endpoint, host, epoch, &shared) => {}
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

async fn monitor(endpoint: ControlEndpoint, host: String, epoch: String, shared: &Shared) {
    let controller = Uuid::new_v4().to_string();
    let ssh = matches!(endpoint, ControlEndpoint::Ssh(_));
    let mut ssh_retry = ssh.then(SshRetry::default);
    let mut manual = shared.ssh_control_reconnect.subscribe();
    let mut retry = RETRY_MIN;
    let mut last_failure = None;
    loop {
        let started = Instant::now();
        let error = synchronize(
            &endpoint,
            &controller,
            &host,
            &epoch,
            shared,
            &mut ssh_retry,
        )
        .await
        .expect_err("a workspace watch ends only on failure");
        shared.update_ui(|ui| ui.inventory_ready = false);

        // Diagnostics contain only a category, never remote error text or titles.
        let reason = match error
            .downcast_ref::<ProtocolFault>()
            .map(|fault| fault.code.as_str())
        {
            Some("ResyncRequired") => "resynchronization required",
            Some("HostMismatch" | "EpochMismatch") => "daemon identity changed",
            Some("LimitExceeded") => "subscription limit reached",
            _ => "connection or protocol failure",
        };
        if last_failure != Some(reason) {
            eprintln!("corvid: workspace synchronization interrupted: {reason}");
            last_failure = Some(reason);
        }
        if ssh {
            let retry = ssh_retry.get_or_insert_with(SshRetry::default);
            if let Some(next) = retry.next_attempt(&error) {
                sleep_until(next).await;
                if Instant::now() < retry.deadline() {
                    continue;
                }
            }
            manual.borrow_and_update();
            shared.update_ui(|ui| ui.inventory_offline = true);
            if manual.changed().await.is_err() {
                return;
            }
            shared.update_ui(|ui| ui.inventory_offline = false);
            ssh_retry = Some(SshRetry::default());
            continue;
        }
        if started.elapsed() >= RETRY_MAX {
            retry = RETRY_MIN;
        }
        sleep(retry).await;
        retry = (retry * 2).min(RETRY_MAX);
    }
}

async fn synchronize(
    endpoint: &ControlEndpoint,
    controller: &str,
    host: &str,
    epoch: &str,
    shared: &Shared,
    retry: &mut Option<SshRetry>,
) -> Result<()> {
    let connection = endpoint.watch(controller, host, || {});
    let mut watch = match retry {
        Some(retry) => timeout_at(retry.deadline(), connection).await??,
        None => connection.await?,
    };
    ensure!(
        watch.state().session().server_epoch == epoch,
        ProtocolFault::new("EpochMismatch", "daemon restarted during attach")
    );
    *retry = None;
    loop {
        publish(shared, Inventory::read(watch.state()));
        watch.next_change().await?;
    }
}

fn publish(shared: &Shared, inventory: Inventory) {
    shared.edit(move |state| {
        Arc::make_mut(&mut state.previews).retain(|pane, _| {
            inventory.terminals.iter().any(|terminal| {
                terminal
                    .location
                    .as_ref()
                    .is_some_and(|location| &location.pane_id == pane)
            })
        });
        let ui = Arc::make_mut(&mut state.ui);
        ui.inventory = Some(Arc::new(inventory));
        ui.inventory_ready = true;
        Changed::Ui
    });
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod terminal_tests;
