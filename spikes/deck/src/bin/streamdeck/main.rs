#[cfg(any(windows, target_os = "macos"))]
mod input;
#[cfg(any(windows, target_os = "macos"))]
mod lcd;
#[cfg(any(windows, target_os = "macos"))]
mod usb;

#[cfg(any(windows, target_os = "macos"))]
use std::{thread, time::Duration};

#[cfg(any(windows, target_os = "macos"))]
use anyhow::{Context, Result, bail};
#[cfg(any(windows, target_os = "macos"))]
use corvid_deck::live::{Capture, Face, LiveDeck};
#[cfg(any(windows, target_os = "macos"))]
use corvid_deck::{Deck, KEY_COUNT, PAGES};
#[cfg(any(windows, target_os = "macos"))]
use corvid_presentation::ipc::DesktopClient;
#[cfg(any(windows, target_os = "macos"))]
use hidapi::{HidApi, HidError};
#[cfg(any(windows, target_os = "macos"))]
use usb::{StreamDeck, supported_devices};
#[cfg(any(windows, target_os = "macos"))]
use uuid::Uuid;

#[cfg(any(windows, target_os = "macos"))]
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(not(any(windows, target_os = "macos")))]
fn main() {
    eprintln!("The Corvid Stream Deck USB adapter currently targets Windows.");
    std::process::exit(1);
}

#[cfg(any(windows, target_os = "macos"))]
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--list" => {
            let api = HidApi::new().context("initialize USB HID")?;
            let devices = supported_devices(&api);
            if devices.is_empty() {
                println!("No supported 15-key Stream Deck connected.");
            }
            for (kind, serial) in devices {
                println!("{kind:?} {serial}");
            }
            Ok(())
        }
        [flag] if flag == "--help" || flag == "-h" => {
            println!(
                "Usage: corvid-streamdeck [--list | --demo | --state-dir PATH | --pipe-instance NAME] [--target CLIENT_ID] [--serial SERIAL]"
            );
            println!("Quit the Elgato Stream Deck app before using this direct USB adapter.");
            println!("The bottom-right key changes pages. Press Ctrl+C to exit.");
            Ok(())
        }
        _ => run(Options::parse(args)?),
    }
}

#[cfg(any(windows, target_os = "macos"))]
struct Options {
    serial: Option<String>,
    target: Option<Uuid>,
    instance: Option<String>,
    state: Option<std::path::PathBuf>,
    demo: bool,
}

#[cfg(any(windows, target_os = "macos"))]
impl Options {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut options = Self {
            serial: None,
            target: None,
            instance: None,
            state: None,
            demo: false,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--serial" => options.serial = Some(args.next().context("missing device serial")?),
                "--target" => {
                    options.target = Some(args.next().context("missing target client ID")?.parse()?)
                }
                "--pipe-instance" | "--socket-dir" => {
                    options.instance = Some(args.next().context("missing pipe instance")?)
                }
                "--state-dir" => {
                    options.state = Some(args.next().context("missing state directory")?.into())
                }
                "--demo" => options.demo = true,
                _ => bail!("unknown option {arg}; use --help"),
            }
        }
        Ok(options)
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn run(options: Options) -> Result<()> {
    let mut selected = options.serial;
    let mut api = HidApi::new().context("initialize USB HID")?;
    let mut deck = Deck::default();
    let mut live =
        if options.demo {
            None
        } else {
            if options.target.is_none() {
                println!("Live display only. Set --target CLIENT_ID to enable focus.");
            }
            let (address, identity) =
                if let Some(instance) = options.instance {
                    #[cfg(windows)]
                    let address = instance;
                    #[cfg(target_os = "macos")]
                    let address = std::path::PathBuf::from(instance);
                    (address, None)
                } else {
                    let state = options
                        .state
                        .map_or_else(corvid_companion::launch::default_state_dir, Ok)?;
                    let scope = corvid_companion::monitor::discover(&state)?.into_iter().next()
                .context("No monitored hosts yet. Connect a host in Corvid Desktop or TUI first.")?;
                    (
                        scope.address,
                        Some((scope.config.host_id, scope.config.controller_id)),
                    )
                };
            Some(LiveDevice {
                client: DesktopClient::start_scoped(address, identity, || {})?,
                deck: LiveDeck::default(),
                target: options.target,
            })
        };
    let renderer = lcd::Renderer::new();
    let mut warned_open_failure = false;
    println!("Waiting for a 15-key Stream Deck. Press Ctrl+C to exit.");
    loop {
        api.refresh_devices().context("scan USB HID devices")?;
        let mut matches = supported_devices(&api)
            .into_iter()
            .filter(|(_, serial)| selected.as_ref().is_none_or(|selected| selected == serial));
        let Some((kind, serial)) = matches.next() else {
            thread::sleep(RESCAN_INTERVAL);
            continue;
        };
        if matches.next().is_some() {
            bail!("multiple matching Stream Decks found. Use --list and --serial SERIAL.");
        }

        let device = match StreamDeck::connect(&api, kind, &serial) {
            Ok(device) => device,
            Err(error) => {
                if !warned_open_failure {
                    eprintln!(
                        "Could not open the Stream Deck ({error}). Quit the Elgato Stream Deck app if it is using the device."
                    );
                    warned_open_failure = true;
                }
                thread::sleep(RESCAN_INTERVAL);
                continue;
            }
        };
        warned_open_failure = false;
        selected = Some(serial);
        println!("Connected {kind:?}. The bottom-right key changes pages.");
        let result = match &mut live {
            Some(live) => live.serve(&device, &renderer),
            None => serve(&device, &mut deck, &renderer),
        };
        match result {
            Err(error) if error.is::<HidError>() => {
                eprintln!("USB connection ended. Waiting for the same Stream Deck.");
                thread::sleep(RESCAN_INTERVAL);
            }
            result => return result,
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
struct LiveDevice {
    client: DesktopClient,
    deck: LiveDeck,
    target: Option<Uuid>,
}

#[cfg(any(windows, target_os = "macos"))]
impl LiveDevice {
    fn refresh(&mut self) {
        let display = self.client.display.borrow_and_update().clone();
        let previous = self.deck.status();
        let result = self.deck.result.clone();
        self.deck.update(display);
        if previous != self.deck.status() {
            println!("{}", self.deck.status());
        }
        if self.deck.result != result
            && let Some(message) = &self.deck.result
        {
            println!("{message}");
        }
        if let (Some(scope), Some(target)) = (self.deck.scope(), self.target)
            && let Some(binding) = self
                .deck
                .bindings()
                .iter()
                .find(|binding| binding.client_id.parse::<Uuid>().ok() == Some(target))
                .cloned()
        {
            self.deck.select_target(scope, binding);
        }
    }

    fn serve(&mut self, device: &StreamDeck, renderer: &lcd::Renderer) -> Result<()> {
        let mut shown: Option<[Face; KEY_COUNT]> = None;
        let mut edges = input::KeyEdges::default();
        let mut shown_captures: [Option<Capture>; KEY_COUNT] = Default::default();
        let mut held: [Option<Capture>; KEY_COUNT] = Default::default();

        loop {
            self.refresh();
            let faces = std::array::from_fn(|index| self.deck.face(index));
            let captures = std::array::from_fn(|index| self.deck.capture(index));
            if shown.as_ref() != Some(&faces) || shown_captures != captures {
                let first = shown.is_none();
                if shown.as_ref() != Some(&faces) {
                    renderer.present_live(device, shown.as_ref(), &faces)?;
                    shown = Some(faces);
                }
                if first || shown_captures != captures {
                    discard_pending_input(device, &mut edges)?;
                    held = Default::default();
                    shown_captures = captures;
                }
            }
            let Some(levels) = device.read_keys(Duration::from_millis(200))? else {
                continue;
            };
            for (index, down) in edges.transitions(&levels)? {
                if down {
                    held[index] = shown_captures[index].clone();
                } else if let Some(capture) = held[index].take() {
                    self.deck.release(&self.client, capture);
                }
            }
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn serve(device: &StreamDeck, deck: &mut Deck, renderer: &lcd::Renderer) -> Result<()> {
    let mut shown = deck.frame();
    renderer.present(device, None, &shown)?;

    let mut keys = input::KeyEdges::default();
    discard_pending_input(device, &mut keys)?;

    loop {
        let Some(levels) = device.read_keys(Duration::from_millis(250))? else {
            continue;
        };
        let presses = keys.apply(&levels, &shown, deck)?;
        if presses.is_empty() {
            continue;
        }

        let next = deck.frame();
        renderer.present(device, Some(&shown), &next)?;
        if next.layout_revision != shown.layout_revision {
            discard_pending_input(device, &mut keys)?;
        }
        shown = next;
        for press in presses {
            let page = &PAGES[press.page];
            let key = page.keys[press.index];
            println!("{} / {} / {}", page.name, key.label, press.outcome.label());
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn discard_pending_input(device: &StreamDeck, keys: &mut input::KeyEdges) -> Result<()> {
    // Queue entries accumulated during a redraw may refer to the old layout.
    for _ in 0..64 {
        let Some(levels) = device.read_keys(Duration::ZERO)? else {
            return Ok(());
        };
        keys.seed(&levels)?;
    }
    bail!("Stream Deck input did not settle after updating the display")
}
