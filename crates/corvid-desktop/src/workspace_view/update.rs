use super::*;
use corvid_update::{InstallProgress, Installation, Installed, Release};

mod state;
mod view;

pub(super) use state::State;
use state::{Failure, Phase};

impl WorkspaceView {
    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if !self.update.start_check() {
            return;
        }
        let check = cx
            .background_executor()
            .spawn(async { corvid_update::check(None) });
        // Keep the task independent of the Settings window's lifetime.
        self.release_update = Some(cx.spawn(async move |view, cx| {
            let result = check.await;
            let _ = view.update(cx, |view, cx| {
                view.update.checked(result);
                view.release_update = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn download_update(&mut self, cx: &mut Context<Self>) {
        let Some((installation, release)) = self.update.start_install() else {
            return;
        };
        // There are only three progress events per installation.
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let install = cx.background_executor().spawn(async move {
            installation.install_with_progress(&release, |stage| {
                let _ = send.send(stage);
            })
        });
        self.release_update = Some(cx.spawn(async move |view, cx| {
            while let Some(stage) = receive.recv().await {
                let _ = view.update(cx, |view, cx| {
                    view.update.phase = Phase::Installing(stage);
                    cx.notify();
                });
            }
            let result = install.await;
            let _ = view.update(cx, |view, cx| {
                view.update.installed(result);
                view.release_update = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn restart_updated_desktop(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.update.phase != Phase::Ready {
            return;
        }
        let Some(installed) = &self.update.installed else {
            return;
        };
        match self.windows.restart(installed, cx) {
            Ok(()) => {}
            Err(error) => {
                self.update.error = Some(Failure {
                    summary: "Could not restart Desktop. Try again or reopen Corvid.",
                    detail: format!("{error:#}"),
                });
                cx.notify();
            }
        }
    }
}
