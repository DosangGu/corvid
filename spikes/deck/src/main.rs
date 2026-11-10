#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("Corvid Deck is a Windows prototype.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::path::PathBuf;

    use corvid_deck::{deck, icons};
    use corvid_presentation::ipc::DesktopClient;
    use gpui::{
        App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px,
        size,
    };

    enum Startup {
        Demo,
        Connect(DesktopClient),
        Managed {
            instance: String,
            state: PathBuf,
            alias: Option<String>,
        },
    }
    let mut args = std::env::args_os().skip(1);
    let mut demo = false;
    let mut connect_only = false;
    let mut instance = "main".to_owned();
    let mut state = None;
    let mut alias = None;
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--help" | "-h") => {
                println!(
                    "Usage: corvid-deck [--demo | --connect-only] [--ssh-alias HOST] [--pipe-instance NAME] [--state-dir DIR]"
                );
                return Ok(());
            }
            Some("--demo") => demo = true,
            Some("--connect-only") => connect_only = true,
            Some("--pipe-instance") => instance = next_text(&mut args, "--pipe-instance")?,
            Some("--state-dir") => {
                state =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("missing --state-dir value")
                    })?))
            }
            Some("--ssh-alias") => alias = Some(next_text(&mut args, "--ssh-alias")?),
            _ => anyhow::bail!("unknown option {flag:?}; use --help"),
        }
    }
    anyhow::ensure!(!(demo && connect_only), "choose one startup mode");
    anyhow::ensure!(
        !(demo || connect_only) || (state.is_none() && alias.is_none()),
        "host setup options require live startup"
    );
    let startup = if demo {
        Startup::Demo
    } else if connect_only {
        Startup::Connect(DesktopClient::start(instance, || {})?)
    } else {
        let state = state.map_or_else(corvid_companion::launch::default_state_dir, Ok)?;
        Startup::Managed {
            instance,
            state,
            alias,
        }
    };
    let lifetime = deck::setup::DesktopLifetime::default();
    let app_lifetime = lifetime.clone();
    Application::new()
        .with_assets(icons::Icons)
        .run(move |cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(1080.), px(680.)), cx);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Corvid Deck".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(620.))),
                ..Default::default()
            };
            match startup {
                Startup::Connect(client) => cx
                    .open_window(options, move |window, cx| {
                        cx.new(|cx| deck::live::LiveView::new(client, window, cx))
                    })
                    .map(|_| ()),
                Startup::Demo => cx
                    .open_window(options, |window, cx| {
                        cx.new(|cx| deck::DeckView::new(window, cx))
                    })
                    .map(|_| ()),
                Startup::Managed {
                    instance,
                    state,
                    alias,
                } => cx
                    .open_window(options, move |window, cx| {
                        cx.new(|cx| {
                            deck::setup::SetupView::new(
                                instance,
                                state,
                                alias,
                                app_lifetime,
                                window,
                                cx,
                            )
                        })
                    })
                    .map(|_| ()),
            }
            .expect("could not open Corvid Deck");
            cx.activate(true);
        });
    lifetime.shutdown();
    Ok(())
}

#[cfg(windows)]
fn next_text(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &str,
) -> anyhow::Result<String> {
    args.next()
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid {flag} value"))
}
