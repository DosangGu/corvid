use std::io::{Write, stdout};
use std::path::Path;

use anyhow::Result;
use corvid_client::{ControlEndpoint, InteractiveConnection};
use corvid_protocol::wire::{BrowseDirectories, DirectoryListing};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::{cursor, execute, terminal};

const VISIBLE: usize = 8;

pub(super) async fn pick(runtime: &Path) -> Result<Option<String>> {
    corvid_client::check_tty()?;
    let endpoint = ControlEndpoint::Local(runtime.to_owned());
    let mut link = connect(&endpoint).await?;
    let initial = query(&mut link, "~", false, false).await?;
    terminal::enable_raw_mode()?;
    let _screen = PickerScreen;
    execute!(
        stdout(),
        terminal::EnterAlternateScreen,
        EnableBracketedPaste,
        cursor::Hide
    )?;

    let mut path = initial.directory.clone();
    let mut listing = Some(initial);
    let mut suggestions = false;
    let mut index = 0;
    let mut error = None;
    loop {
        draw(&path, listing.as_ref(), index, error.as_deref())?;
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Paste(text) => {
                if path.len().saturating_add(text.len()) <= 4096
                    && !text.chars().any(char::is_control)
                {
                    path.push_str(&text);
                    listing = None;
                }
                continue;
            }
            _ => continue,
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        error = None;
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                path.clear();
                listing = None;
            }
            KeyCode::Char(ch)
                if !ch.is_control() && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if path.len() + ch.len_utf8() <= 4096 {
                    path.push(ch);
                    listing = None;
                }
            }
            KeyCode::Backspace => {
                path.pop();
                listing = None;
            }
            KeyCode::Up => index = index.saturating_sub(1),
            KeyCode::Down => {
                if let Some(list) = &listing {
                    index = (index + 1).min(list.entries.len().saturating_sub(1));
                }
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Left | KeyCode::Enter => {
                let complete = key.code == KeyCode::Tab && listing.is_none();
                let validate = key.code == KeyCode::Enter;
                if let Some(list) = &listing {
                    if validate && !suggestions {
                        return Ok(Some(list.directory.clone()));
                    }
                    if validate {
                        // A prefix lookup lists siblings. Validate the typed path.
                    } else if key.code == KeyCode::Left {
                        let Some(parent) = &list.parent else { continue };
                        path.clone_from(parent);
                    } else {
                        let Some(entry) = list.entries.get(index) else {
                            continue;
                        };
                        path.clone_from(&entry.path);
                    }
                } else if key.code == KeyCode::Left {
                    continue;
                }
                match lookup(&endpoint, &mut link, &path, complete, validate).await {
                    Ok(list) if validate => return Ok(Some(list.directory)),
                    Ok(list) => {
                        suggestions = complete;
                        if !complete {
                            path.clone_from(&list.directory);
                        }
                        listing = Some(list);
                        index = 0;
                    }
                    Err(fault) => {
                        error = Some(fault.to_string());
                        listing = None;
                    }
                }
            }
            _ => {}
        }
    }
}

async fn connect(endpoint: &ControlEndpoint) -> Result<InteractiveConnection> {
    endpoint
        .interactive(vec![corvid_protocol::WORKSPACE_DIRECTORY_CAPABILITY.into()])
        .await
}

async fn query(
    link: &mut InteractiveConnection,
    path: &str,
    complete: bool,
    validate_only: bool,
) -> Result<DirectoryListing> {
    link.browse_directories(BrowseDirectories {
        path: path.into(),
        complete,
        show_hidden: false,
        validate_only,
    })
    .await
}

async fn lookup(
    endpoint: &ControlEndpoint,
    link: &mut InteractiveConnection,
    path: &str,
    complete: bool,
    validate_only: bool,
) -> Result<DirectoryListing> {
    match query(link, path, complete, validate_only).await {
        Err(error)
            if error
                .downcast_ref::<corvid_protocol::ProtocolFault>()
                .is_none() =>
        {
            *link = connect(endpoint).await?;
            query(link, path, complete, validate_only).await
        }
        result => result,
    }
}

fn draw(
    path: &str,
    listing: Option<&DirectoryListing>,
    index: usize,
    error: Option<&str>,
) -> Result<()> {
    let mut output = stdout().lock();
    execute!(
        output,
        cursor::MoveTo(0, 0),
        terminal::Clear(terminal::ClearType::All)
    )?;
    write!(output, "Workspace folder\r\n{path}\r\n\r\n")?;
    if let Some(listing) = listing {
        for (i, entry) in listing
            .entries
            .iter()
            .enumerate()
            .skip(index / VISIBLE * VISIBLE)
            .take(VISIBLE)
        {
            write!(
                output,
                "{} {}\r\n",
                if i == index { ">" } else { " " },
                entry.name
            )?;
        }
    }
    if let Some(error) = error {
        write!(output, "\r\n{error}\r\n")?;
    }
    write!(
        output,
        "\r\nTab completes · ↑/↓ select · → opens · ← goes up\r\nEnter uses folder · Ctrl+U clears · Esc cancels"
    )?;
    output.flush()?;
    Ok(())
}

struct PickerScreen;

impl Drop for PickerScreen {
    fn drop(&mut self) {
        let _ = execute!(
            stdout(),
            DisableBracketedPaste,
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
        let _ = terminal::disable_raw_mode();
    }
}
