use std::time::Duration;

use thiserror::Error;

use crate::frame::CONTROL_LIMIT;
use crate::wire::envelope::Body;
use crate::wire::{AttachRole, Envelope, Hello, HelloResult, Limits, RpcError};

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 0;
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

const MAX_CAPABILITIES: usize = 64;
const MAX_MAJOR_VERSIONS: usize = 16;
const WRITER_POLICY: &str = "exclusive_or_reject";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerRole {
    Tui,
    Control,
    Collector,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{code}: {message}")]
pub struct ProtocolFault {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub supported_major_versions: Vec<u32>,
}

impl ProtocolFault {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            retryable: false,
            supported_major_versions: Vec::new(),
        }
    }

    fn invalid(message: &str) -> Self {
        Self::new("InvalidMessage", message)
    }

    fn incompatible() -> Self {
        let mut fault = Self::new(
            "ProtocolMismatch",
            "no compatible protocol version; update client or daemon manually",
        );
        fault.supported_major_versions.push(PROTOCOL_MAJOR);
        fault
    }

    pub fn into_wire(self) -> RpcError {
        RpcError {
            code: self.code,
            message: self.message,
            retryable: self.retryable,
            supported_major_versions: self.supported_major_versions,
        }
    }

    pub fn from_wire(error: &RpcError) -> Self {
        if !valid_id(&error.code)
            || error.message.len() > 1024
            || error.message.chars().any(char::is_control)
            || error.supported_major_versions.len() > MAX_MAJOR_VERSIONS
        {
            return Self::invalid("invalid error payload");
        }

        Self {
            code: error.code.clone(),
            message: error.message.clone(),
            retryable: error.retryable,
            supported_major_versions: error.supported_major_versions.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub role: PeerRole,
    pub host_id: String,
    pub server_epoch: String,
    pub controller_id: Option<String>,
    pub capabilities: Vec<String>,
}

impl Session {
    pub fn envelope(&self, request_id: Option<String>, body: Body) -> Envelope {
        Envelope {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            request_id,
            host_id: Some(self.host_id.clone()),
            server_epoch: Some(self.server_epoch.clone()),
            body: Some(body),
            ..Default::default()
        }
    }

    pub fn validate_scope(&self, envelope: &Envelope) -> Result<(), ProtocolFault> {
        if envelope.protocol_major != PROTOCOL_MAJOR || envelope.protocol_minor != PROTOCOL_MINOR {
            return Err(ProtocolFault::incompatible());
        }
        check_identity(envelope, &self.host_id, &self.server_epoch, false)
    }

    pub fn validate_request<'a>(&self, request: &'a Envelope) -> Result<&'a Body, ProtocolFault> {
        self.validate_scope(request)?;
        require_request_id(request)?;
        if request.sequence.is_some() || request.subscription_id.is_some() {
            return Err(ProtocolFault::invalid("request has event metadata"));
        }

        let body = known_body(request)?;
        match (self.role, body) {
            (PeerRole::Tui | PeerRole::Control, Body::Ping(_))
            | (PeerRole::Collector, Body::CollectHook(_))
            | (
                PeerRole::Control,
                Body::GetHost(_)
                | Body::GetConnectionStatus(_)
                | Body::ListWorkspaces(_)
                | Body::ListAgents(_)
                | Body::ListClients(_)
                | Body::GetSnapshot(_)
                | Body::SnapshotWatch(_)
                | Body::RegisterClientBinding(_)
                | Body::UnregisterClientBinding(_)
                | Body::AcknowledgeAttention(_)
                | Body::FocusAgent(_),
            ) => Ok(body),
            (PeerRole::Tui, Body::Attach(attach)) => {
                match AttachRole::try_from(attach.requested_role) {
                    Ok(AttachRole::Writer) => Ok(body),
                    Ok(AttachRole::Unspecified) => {
                        Err(ProtocolFault::invalid("missing attach role"))
                    }
                    _ => Err(ProtocolFault::new(
                        "UnsupportedCapability",
                        "attach role is unavailable",
                    )),
                }
            }
            (
                PeerRole::Tui,
                Body::InputChunk(_)
                | Body::PasteBegin(_)
                | Body::PasteChunk(_)
                | Body::PasteEnd(_)
                | Body::ResizeViewport(_)
                | Body::SelectView(_)
                | Body::ClaimWorkspaceWriter(_)
                | Body::IssuePairingCode(_)
                | Body::RedrawView(_)
                | Body::FrameApplied(_)
                | Body::Detach(_),
            ) => Ok(body),
            _ => Err(ProtocolFault::new(
                "UnsupportedCapability",
                "message is unavailable for this connection role",
            )),
        }
    }
}

pub fn hello(
    request_id: String,
    controller_id: Option<String>,
    required_capabilities: Vec<String>,
) -> Envelope {
    Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        request_id: Some(request_id),
        body: Some(Body::Hello(Hello {
            controller_id,
            supported_major_versions: vec![PROTOCOL_MAJOR],
            required_capabilities,
        })),
        ..Default::default()
    }
}

pub fn accept_hello(
    request: &Envelope,
    role: PeerRole,
    host_id: &str,
    server_epoch: &str,
    capabilities: &[&str],
) -> Result<(Session, Envelope), ProtocolFault> {
    let request_id = require_request_id(request)?;
    let hello = validate_hello(request, role)?;
    check_identity(request, host_id, server_epoch, true)?;
    check_required(&hello.required_capabilities, capabilities)?;

    let session = Session {
        role,
        host_id: host_id.to_owned(),
        server_epoch: server_epoch.to_owned(),
        controller_id: hello.controller_id.clone(),
        capabilities: capabilities.iter().map(ToString::to_string).collect(),
    };
    let result = HelloResult {
        capabilities: session.capabilities.clone(),
        limits: Some(Limits {
            control_frame_bytes: CONTROL_LIMIT as u32,
            command_timeout_ms: COMMAND_TIMEOUT.as_millis() as u32,
        }),
        writer_policy: WRITER_POLICY.to_owned(),
    };
    let reply = session.envelope(Some(request_id.to_owned()), Body::HelloResult(result));
    Ok((session, reply))
}

pub fn check_hello_result(
    request: &Envelope,
    reply: &Envelope,
    role: PeerRole,
) -> Result<Session, ProtocolFault> {
    let hello = validate_hello(request, role)?;
    if require_request_id(reply)? != require_request_id(request)? {
        return Err(ProtocolFault::invalid("HelloResult request ID mismatch"));
    }
    if let Body::Error(error) = known_body(reply)? {
        return Err(ProtocolFault::from_wire(error));
    }
    if reply.protocol_major != PROTOCOL_MAJOR || reply.protocol_minor != PROTOCOL_MINOR {
        return Err(ProtocolFault::incompatible());
    }

    let Body::HelloResult(result) = known_body(reply)? else {
        return Err(ProtocolFault::invalid("expected HelloResult"));
    };
    if !valid_capabilities(&result.capabilities)
        || result.writer_policy != WRITER_POLICY
        || !valid_limits(result.limits.as_ref())
        || reply.sequence.is_some()
        || reply.subscription_id.is_some()
    {
        return Err(ProtocolFault::invalid("invalid HelloResult"));
    }
    check_required(&hello.required_capabilities, &result.capabilities)?;

    let host_id = require_id(reply.host_id.as_deref(), "missing or invalid host ID")?;
    let server_epoch = require_id(
        reply.server_epoch.as_deref(),
        "missing or invalid server epoch",
    )?;
    check_identity(request, host_id, server_epoch, true)?;
    Ok(Session {
        role,
        host_id: host_id.to_owned(),
        server_epoch: server_epoch.to_owned(),
        controller_id: hello.controller_id.clone(),
        capabilities: result.capabilities.clone(),
    })
}

fn validate_hello(request: &Envelope, role: PeerRole) -> Result<&Hello, ProtocolFault> {
    let Body::Hello(hello) = known_body(request)? else {
        return Err(ProtocolFault::invalid("expected Hello"));
    };
    if request.protocol_major == 0
        || request.sequence.is_some()
        || request.subscription_id.is_some()
        || hello.supported_major_versions.is_empty()
        || hello.supported_major_versions.len() > MAX_MAJOR_VERSIONS
        || hello.supported_major_versions.contains(&0)
        || !hello
            .supported_major_versions
            .contains(&request.protocol_major)
        || !valid_capabilities(&hello.required_capabilities)
    {
        return Err(ProtocolFault::invalid("invalid Hello fields"));
    }
    if !hello.supported_major_versions.contains(&PROTOCOL_MAJOR) {
        return Err(ProtocolFault::incompatible());
    }
    if role == PeerRole::Control || hello.controller_id.is_some() {
        require_id(
            hello.controller_id.as_deref(),
            "missing or invalid controller ID",
        )?;
    }
    Ok(hello)
}

fn valid_limits(limits: Option<&Limits>) -> bool {
    limits.is_some_and(|limits| {
        limits.control_frame_bytes == CONTROL_LIMIT as u32
            && limits.command_timeout_ms == COMMAND_TIMEOUT.as_millis() as u32
    })
}

fn check_identity(
    envelope: &Envelope,
    host_id: &str,
    server_epoch: &str,
    allow_omission: bool,
) -> Result<(), ProtocolFault> {
    for (value, expected, code) in [
        (envelope.host_id.as_deref(), host_id, "HostMismatch"),
        (
            envelope.server_epoch.as_deref(),
            server_epoch,
            "EpochMismatch",
        ),
    ] {
        if value.is_none() && allow_omission {
            continue;
        }

        if require_id(value, "missing or invalid connection identity")? != expected {
            return Err(ProtocolFault::new(code, "connection identity has changed"));
        }
    }
    Ok(())
}

fn check_required(required: &[String], available: &[impl AsRef<str>]) -> Result<(), ProtocolFault> {
    if required
        .iter()
        .any(|required| !available.iter().any(|item| item.as_ref() == required))
    {
        return Err(ProtocolFault::new(
            "UnsupportedCapability",
            "required capability is unavailable",
        ));
    }
    Ok(())
}

fn valid_capabilities(capabilities: &[String]) -> bool {
    capabilities.len() <= MAX_CAPABILITIES
        && capabilities.iter().all(|capability| {
            !capability.is_empty()
                && capability.len() <= 64
                && capability
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn known_body(envelope: &Envelope) -> Result<&Body, ProtocolFault> {
    envelope.body.as_ref().ok_or_else(|| {
        ProtocolFault::new(
            "UnsupportedCapability",
            "message body is missing or unknown",
        )
    })
}

fn require_request_id(envelope: &Envelope) -> Result<&str, ProtocolFault> {
    require_id(
        envelope.request_id.as_deref(),
        "missing or invalid request ID",
    )
}

fn require_id<'a>(value: Option<&'a str>, error: &str) -> Result<&'a str, ProtocolFault> {
    value
        .filter(|id| valid_id(id))
        .ok_or_else(|| ProtocolFault::invalid(error))
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}
