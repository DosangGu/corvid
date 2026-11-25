use std::collections::{BTreeMap, VecDeque};

use base64::{Engine, engine::general_purpose};
use corvid_protocol::wire::{ProgramPhase, ProgramStatus, ProgramWaitKind};

const FRAME_LIMIT: usize = 4096;
const RECORD_LIMIT: usize = 64;
const QUERY_REPLY: &[u8] =
    b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

#[derive(Clone, Copy)]
pub(super) enum Event {
    Osc,
    Prompt,
    Reset,
}

#[derive(Default)]
pub(super) struct Programs {
    tail: VecDeque<u8>,
    awaiting_st: Option<Command>,
    records: Vec<(String, Report)>,
    received_status: bool,
    replies: Vec<u8>,
}

enum Command {
    Query,
    Clear(String),
    Report(String, Report),
    Progress(Option<Report>),
}

#[derive(Clone, Copy)]
enum Phase {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

#[derive(Clone, Copy, Default)]
enum Wait {
    #[default]
    Unspecified,
    Permission,
    Question,
    Auth,
}

struct Report {
    phase: Phase,
    wait: Wait,
    progress: Option<u8>,
    app: String,
    title: String,
    message: String,
}

impl Programs {
    pub(super) fn advance(&mut self, bytes: &[u8], event: Option<Event>) {
        if bytes.is_empty() {
            return;
        }
        if let Some(command) = self.awaiting_st.take()
            && bytes.first() == Some(&b'\\')
        {
            self.apply(command);
        }

        let bytes = &bytes[bytes.len().saturating_sub(FRAME_LIMIT)..];
        let excess = (self.tail.len() + bytes.len()).saturating_sub(FRAME_LIMIT);
        self.tail.drain(..excess);
        self.tail.extend(bytes);
        match event {
            Some(Event::Osc) => {
                let Some((command, complete)) = self.decode() else {
                    return;
                };
                if complete {
                    self.apply(command);
                } else {
                    self.awaiting_st = Some(command);
                }
            }
            Some(Event::Prompt) => self.expire_active(),
            Some(Event::Reset) => {
                self.records.clear();
                self.received_status = false;
            }
            None => {}
        }
    }

    fn decode(&mut self) -> Option<(Command, bool)> {
        let (terminator, body) = self.tail.make_contiguous().split_last()?;
        let complete = match terminator {
            b'\x07' => true,
            b'\x1b' => false,
            _ => return None,
        };
        let start = body.iter().rposition(|byte| *byte == b'\x1b')?;
        let body = body[start..].strip_prefix(b"\x1b]")?;
        let raw = std::str::from_utf8(body).ok()?;
        if raw.bytes().any(|byte| byte.is_ascii_control()) || body.len() + 4 > FRAME_LIMIT {
            return None;
        }
        let command = raw
            .strip_prefix("7501;")
            .and_then(parse_status)
            .or_else(|| raw.strip_prefix("9;4;").and_then(parse_progress))?;
        Some((command, complete))
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Query => {
                if self.replies.len() + QUERY_REPLY.len() <= super::super::MAX_REPLY_BYTES {
                    self.replies.extend_from_slice(QUERY_REPLY);
                }
            }
            Command::Clear(id) => {
                self.use_7501();
                self.records.retain(|(existing, _)| {
                    !id.is_empty()
                        && existing != &id
                        && !existing
                            .strip_prefix(&id)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                });
            }
            Command::Report(id, report) => {
                self.use_7501();
                self.replace(id, report);
            }
            Command::Progress(report) if !self.received_status => {
                if let Some(report) = report {
                    self.replace(String::new(), report);
                } else {
                    self.records.clear();
                }
            }
            Command::Progress(_) => {}
        }
    }

    fn use_7501(&mut self) {
        if !self.received_status {
            self.records.clear();
            self.received_status = true;
        }
    }

    fn replace(&mut self, id: String, report: Report) {
        self.records.retain(|(existing, _)| existing != &id);
        if self.records.len() == RECORD_LIMIT {
            self.records.remove(0);
        }
        self.records.push((id, report));
    }

    pub(super) fn expire_active(&mut self) {
        self.records
            .retain(|(_, report)| !matches!(report.phase, Phase::Working | Phase::Blocked));
    }

    pub(super) fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    pub(super) fn summary(&self) -> Option<ProgramStatus> {
        let (id, report) = self
            .records
            .iter()
            .max_by_key(|(_, report)| report.phase.priority())?;
        let app = if report.app.is_empty() {
            let mut parent = id.as_str();
            loop {
                if parent.is_empty() {
                    break String::new();
                }
                parent = parent.rsplit_once('/').map_or("", |(head, _)| head);
                if let Some((_, report)) = self
                    .records
                    .iter()
                    .find(|(id, report)| id == parent && !report.app.is_empty())
                {
                    break report.app.clone();
                }
            }
        } else {
            report.app.clone()
        };
        Some(ProgramStatus {
            phase: report.phase.wire() as i32,
            wait_kind: match report.wait {
                Wait::Unspecified => ProgramWaitKind::Unspecified,
                Wait::Permission => ProgramWaitKind::Permission,
                Wait::Question => ProgramWaitKind::Question,
                Wait::Auth => ProgramWaitKind::Auth,
            } as i32,
            progress: report.progress.map(u32::from),
            app,
            title: report.title.clone(),
            message: report.message.clone(),
            osc_7501: self.received_status,
        })
    }
}

impl Phase {
    fn wire(self) -> ProgramPhase {
        match self {
            Self::Idle => ProgramPhase::Idle,
            Self::Working => ProgramPhase::Working,
            Self::Blocked => ProgramPhase::Blocked,
            Self::Done => ProgramPhase::Done,
            Self::Error => ProgramPhase::Error,
        }
    }

    fn priority(self) -> u8 {
        self.wire().summary_priority()
    }
}

fn parse_status(raw: &str) -> Option<Command> {
    if raw == "?" {
        return Some(Command::Query);
    }
    let mut fields = BTreeMap::new();
    for pair in raw.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.len() > 16 {
            return None;
        }
        if !key.is_empty()
            && key.bytes().all(|byte| byte.is_ascii_lowercase())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.,+/=-".contains(&byte))
        {
            fields.insert(key, value);
        }
    }
    let id = fields.get("id").copied();
    if id.is_some_and(|id| !valid_id(id)) {
        return None;
    }
    let app = fields.get("app").copied().unwrap_or_default();
    if app.len() > 32 {
        return None;
    }
    let title = decode_text(fields.get("title").copied().unwrap_or_default(), 256, 192)?;
    let message = decode_text(fields.get("msg").copied().unwrap_or_default(), 2732, 2048)?;
    let id = id.unwrap_or_default().to_owned();
    let phase = match *fields.get("state")? {
        "clear" => return Some(Command::Clear(id)),
        "idle" => Phase::Idle,
        "working" => Phase::Working,
        "blocked" => Phase::Blocked,
        "done" => Phase::Done,
        "error" => Phase::Error,
        _ => return None,
    };
    let wait = match (phase, fields.get("kind").copied()) {
        (Phase::Blocked, Some("permission")) => Wait::Permission,
        (Phase::Blocked, Some("question")) => Wait::Question,
        (Phase::Blocked, Some("auth")) => Wait::Auth,
        _ => Wait::Unspecified,
    };
    let progress = match phase {
        Phase::Working | Phase::Blocked => fields.get("progress").and_then(|value| percent(value)),
        _ => None,
    };
    Some(Command::Report(
        id,
        Report {
            phase,
            wait,
            progress,
            app: if app
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.+".contains(&byte) && byte != b'+')
            {
                app.to_owned()
            } else {
                String::new()
            },
            title,
            message,
        },
    ))
}

fn valid_id(id: &str) -> bool {
    let parts = id.split('/');
    id.len() <= 128
        && parts.clone().count() <= 8
        && parts.into_iter().all(|part| {
            (1..=32).contains(&part.len())
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte))
        })
}

fn decode_text(encoded: &str, encoded_limit: usize, decoded_limit: usize) -> Option<String> {
    if encoded.len() > encoded_limit {
        return None;
    }
    let engine = if encoded.ends_with('=') {
        &general_purpose::STANDARD
    } else {
        &general_purpose::STANDARD_NO_PAD
    };
    let text = String::from_utf8(engine.decode(encoded).ok()?).ok()?;
    if text.len() > decoded_limit || text.chars().any(char::is_control) {
        return None;
    }
    Some(
        text.chars()
            .filter(|ch| !super::super::is_bidi_control(*ch))
            .collect(),
    )
}

fn percent(value: &str) -> Option<u8> {
    let value = value.parse::<u8>().ok()?;
    (value <= 100).then_some(value)
}

fn parse_progress(value: &str) -> Option<Command> {
    let (mode, value) = value.split_once(';').unwrap_or((value, ""));
    let phase = match mode {
        "0" => return Some(Command::Progress(None)),
        "1" | "3" => Phase::Working,
        "2" => Phase::Error,
        "4" => Phase::Blocked,
        _ => return None,
    };
    Some(Command::Progress(Some(Report {
        phase,
        wait: Wait::Unspecified,
        progress: if mode == "3" { None } else { percent(value) },
        app: String::new(),
        title: String::new(),
        message: String::new(),
    })))
}
