use super::*;

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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (stop, stopped) = oneshot::channel();
        let (finished, receive_finished) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("corvid-desktop-inventory".into())
            .spawn(move || {
                runtime.block_on(async {
                    tokio::select! {
                        _ = stopped => {}
                        _ = monitor(endpoint, host, epoch, &shared) => {}
                    }
                });
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
    let mut retry = RETRY_MIN;
    let mut last_failure = None;
    loop {
        let started = Instant::now();
        let error = synchronize(&endpoint, &controller, &host, &epoch, shared)
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
        if started.elapsed() >= RETRY_MAX {
            retry = RETRY_MIN;
        }
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(RETRY_MAX);
    }
}

async fn synchronize(
    endpoint: &ControlEndpoint,
    controller: &str,
    host: &str,
    epoch: &str,
    shared: &Shared,
) -> Result<()> {
    let mut watch = endpoint.watch(controller, host, || {}).await?;
    ensure!(
        watch.state().session().server_epoch == epoch,
        ProtocolFault::new("EpochMismatch", "daemon restarted during attach")
    );
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
