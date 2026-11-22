use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use corvid_presentation::ipc::DesktopClient;
use gpui::{AppContext, Application};
use uuid::Uuid;

use crate::background::Background;
use crate::shell;
use crate::ui::{DeckAssets, DeckView};
use crate::ui_signal::UiSignal;

pub(crate) struct Session {
    pub client: DesktopClient,
    pub background: Background,
    pub signal: UiSignal,
    pub catalog: (PathBuf, Uuid),
}

pub(crate) fn run(session: Session, preferences: shell::Window) {
    Application::new().with_assets(DeckAssets).run(move |cx| {
        shell::prepare();
        #[cfg(target_os = "macos")]
        cx.on_app_quit(|_| {
            crate::platform::shutdown_on_main();
            async {}
        })
        .detach();
        let session = Rc::new(session);
        let mut updates = session.signal.subscribe();
        let options = shell::options(&preferences, cx);
        let view_session = session.clone();
        let window = cx
            .open_window(options, move |window, cx| {
                cx.new(|cx| DeckView::new(view_session, preferences, window, cx))
            })
            .expect("could not open Corvid Agent Deck");

        // The application owns the connection and worker. Hiding a surface
        // leaves both running and keeps the desktop registration alive.
        cx.spawn(async move |cx| {
            loop {
                let tick = cx.background_executor().timer(Duration::from_secs(1));
                corvid_async::select! {
                    result = updates.changed() => if result.is_err() { break },
                    _ = tick => {}
                }
                let result = cx.update(|cx| {
                    if session.signal.quitting()
                        || window
                            .update(cx, |view, window, cx| {
                                view.refresh(session.signal.take_presentation(), window, cx);
                            })
                            .is_err()
                    {
                        cx.quit();
                    }
                });
                if result.is_err() || session.signal.quitting() {
                    break;
                }
            }
        })
        .detach();
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
}
