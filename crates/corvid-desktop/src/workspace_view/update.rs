use super::*;
use corvid_update::{Installation, Installed, Release};

mod state;
mod view;

pub(super) use state::State;
use state::{Failure, Phase};

impl WorkspaceView {
    pub(crate) fn show_pending_cleanup(&mut self, pending: bool, cx: &mut Context<Self>) {
        if self.update.cleanup_pending != pending {
            self.update.cleanup_pending = pending;
            cx.notify();
        }
    }

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
        let install = cx
            .background_executor()
            .spawn(async move { installation.prepare(&release, |_| {}) });
        self.release_update = Some(cx.spawn(async move |view, cx| {
            let result = install.await;
            let _ = view.update(cx, |view, cx| {
                view.update.prepared(result);
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
        let owner = cx.entity().downgrade();
        let restart_failed = move |error: anyhow::Error, cx: &mut App| {
            let detail = format!("{error:#}");
            if owner
                .update(cx, |view, cx| {
                    view.update.error = Some(Failure {
                        summary: "Could not restart Desktop. Try again or reopen Corvid.",
                        detail: detail.clone(),
                    });
                    cx.notify();
                })
                .is_err()
            {
                eprintln!("Could not restart Desktop: {detail}");
            }
        };
        if let Some(prepared) = self.update.prepared.clone() {
            self.update.phase = Phase::Installing;
            self.update.error = None;
            let owner = cx.entity().downgrade();
            self.windows.apply_update(
                move || prepared.apply(),
                move |result, cx| {
                    if let Err(error) = &result
                        && owner.upgrade().is_none()
                    {
                        eprintln!("Could not apply the Desktop update: {error:#}");
                    }
                    let _ = owner.update(cx, |view, cx| {
                        view.update.installed(result);
                        view.release_update = None;
                        cx.notify();
                    });
                },
                restart_failed,
                cx,
            );
            cx.notify();
        } else if let Some(installed) = &self.update.installed {
            self.windows
                .restart(Rc::clone(installed), restart_failed, cx);
        }
    }
}
