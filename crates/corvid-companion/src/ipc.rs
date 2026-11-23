use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use corvid_async::{mpsc, oneshot};
use corvid_client::ControlEndpoint;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use corvid_protocol::unix::{RuntimeDir, SocketRole};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Envelope, GetPresentationSlots, Pong, PresentationSlots};
use corvid_protocol::{
    COMMAND_TIMEOUT, FrameRole, PROTOCOL_MAJOR, PROTOCOL_MINOR, PeerRole, ProtocolFault, Session,
    accept_hello_with_profiles,
};
use uuid::Uuid;

use crate::Store;
use crate::service::{Call, Service};

mod native;

pub const PRESENTATION_CAPABILITY: &str = "presentation_slots.v1";
pub const PRESENTATION_PAGE_SIZE: u32 = 32;
pub const PRESENTATION_ATTENTION_CAPABILITY: &str = "presentation_attention.v1";
pub const PRESENTATION_ATTENTION_LIMIT: usize = 512;
const LIST_HOSTS_CAPABILITY: &str = "list_hosts.v1";
pub(crate) const MONITOR_RECONNECT_CAPABILITY: &str = "monitor_reconnect.v1";
#[cfg(windows)]
pub const WEZTERM_CAPABILITY: &str = "wezterm_local.v1";
#[cfg(windows)]
pub const WEZTERM_SELECTION_CAPABILITY: &str = "wezterm_selection.v1";
const CAPABILITIES: &[&str] = &[
    crate::monitor::CAPABILITY,
    crate::monitor::EPOCH_RETIREMENT_CAPABILITY,
    crate::monitor::DECK_PRIORITY_CAPABILITY,
    crate::monitor::PROCESS_EXIT_RETIREMENT_CAPABILITY,
    PRESENTATION_CAPABILITY,
    PRESENTATION_ATTENTION_CAPABILITY,
    LIST_HOSTS_CAPABILITY,
    MONITOR_RECONNECT_CAPABILITY,
    #[cfg(windows)]
    WEZTERM_CAPABILITY,
    #[cfg(windows)]
    WEZTERM_SELECTION_CAPABILITY,
];
const ROLE: FrameRole = FrameRole::StructuredOnly;
const MAX_PEERS: usize = 8;
const MAX_CALLS: usize = 8;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(windows))]
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
// Windows focus also verifies the pane around the remote command and may
// wait briefly for the verified window to reach the foreground.
#[cfg(windows)]
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub type PresentationAddress = Path;
#[cfg(windows)]
pub type PresentationAddress = str;

/// Serve the configured companion on the caller's Compio runtime.
pub async fn run_native(
    runtime: &PresentationAddress,
    state: &Path,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let store = Store::open(state)?;
    let endpoint = store.config().control_endpoint()?;
    serve_runtime(runtime, store, endpoint, shutdown).await
}

pub async fn run(
    runtime: &PresentationAddress,
    state: &Path,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let store = Store::open(state)?;
    let endpoint = store.config().control_endpoint()?;
    run_service(runtime, store, endpoint, shutdown).await
}

/// Serve the built-in Agents window using the endpoint that Desktop verified.
pub async fn run_for_endpoint(
    runtime: &PresentationAddress,
    state: &Path,
    endpoint: ControlEndpoint,
    host: Uuid,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let store = Store::open(state)?;
    ensure!(
        store.config().host_id == host,
        "companion state belongs to a different host"
    );
    run_service(runtime, store, endpoint, shutdown).await
}

async fn run_service(
    runtime: &PresentationAddress,
    store: Store,
    endpoint: ControlEndpoint,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    return native::run(runtime, store, endpoint, shutdown).await;
}

async fn serve_runtime(
    runtime: &PresentationAddress,
    store: Store,
    endpoint: ControlEndpoint,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let host = store.config().host_id.to_string();
    let epoch = Uuid::new_v4().to_string();
    let mut service = Service::new(store, endpoint);

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let (directory, instance);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let listener = {
        directory = RuntimeDir::create_all(runtime)?;
        instance = directory.lock_instance()?;
        instance.bind_compio(SocketRole::Presentation).await?
    };
    #[cfg(windows)]
    let listener =
        corvid_async::Mutex::new(corvid_protocol::windows::CompioPipeListener::bind(runtime)?);
    let (sender, calls) = mpsc::channel(MAX_CALLS);

    let incoming = native::accept(&listener, sender, &host, &epoch);
    let result = corvid_async::select! {
        result = incoming => result,
        result = service.run(calls) => result,
        _ = shutdown => Ok(()),
    };
    result.and(service.flush_status())
}

fn negotiate_peer(
    request: &Envelope,
    host: &str,
    epoch: &str,
) -> Result<(Session, Envelope), ProtocolFault> {
    accept_hello_with_profiles(
        request,
        PeerRole::Presentation,
        host,
        epoch,
        CAPABILITIES,
        &[],
        Some(env!("CARGO_PKG_VERSION")),
    )
}

fn rejection(request: Envelope, fault: &ProtocolFault) -> Envelope {
    Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        request_id: request
            .request_id
            .filter(|id| id.len() <= 256 && !id.chars().any(char::is_control)),
        body: Some(Body::Error(fault.clone().into_wire())),
        ..Default::default()
    }
}

async fn reply(
    session: &Session,
    calls: &mpsc::Sender<Call>,
    request: &mut Envelope,
) -> (Body, bool) {
    match session.validate_request(request) {
        Ok(Body::StopAgentMonitor(_)) => (Body::Pong(Pong {}), true),
        Ok(Body::Ping(_)) => (Body::Pong(Pong {}), false),
        Ok(_) => (
            dispatch(calls, request.body.take().expect("validated body"))
                .await
                .unwrap_or_else(|fault| Body::Error(fault.into_wire())),
            false,
        ),
        Err(fault) => (Body::Error(fault.into_wire()), false),
    }
}

async fn dispatch(calls: &mpsc::Sender<Call>, body: Body) -> Result<Body, ProtocolFault> {
    let (reply, receive) = oneshot::channel();
    calls
        .try_send(Call { body, reply })
        .map_err(|_| ProtocolFault::new("LimitExceeded", "companion request queue unavailable"))?;
    use compio::time::timeout;
    timeout(CALL_TIMEOUT, receive)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_else(|| {
            Err(ProtocolFault::new(
                "OutcomeUnknown",
                "companion result was unavailable",
            ))
        })
}

pub struct PresentationClient(corvid_client::CompioPresentation);

impl PresentationClient {
    pub async fn connect(runtime: &PresentationAddress) -> Result<Self> {
        return corvid_client::CompioPresentation::connect(runtime, PRESENTATION_CAPABILITY)
            .await
            .map(Self);
    }

    pub async fn connect_if_present(runtime: &PresentationAddress) -> Result<Option<Self>> {
        match Self::connect(runtime).await {
            Ok(client) => Ok(Some(client)),
            Err(error) if presentation_absent(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn session(&self) -> &Session {
        self.0.session()
    }

    pub async fn reconnect_monitor(&mut self) -> Result<()> {
        ensure!(
            self.session()
                .capabilities
                .iter()
                .any(|capability| capability == MONITOR_RECONNECT_CAPABILITY),
            "companion does not support manual reconnect"
        );
        let reply = self
            .request(Body::ReconnectAgentMonitor(
                corvid_protocol::wire::ReconnectAgentMonitor {},
            ))
            .await?;
        ensure!(
            matches!(reply, Body::Pong(_)),
            "unexpected reconnect response"
        );
        Ok(())
    }

    pub async fn slots(&mut self, config: Option<&crate::Config>) -> Result<PresentationSlots> {
        let reply = self
            .request(Body::GetPresentationSlots(GetPresentationSlots::default()))
            .await?;
        let Body::PresentationSlots(slots) = reply else {
            anyhow::bail!("unexpected slot response");
        };
        if let Some(config) = config {
            ensure!(
                self.session().host_id == config.host_id.to_string()
                    && slots.controller_id == config.controller_id.to_string(),
                "companion identity mismatch"
            );
        }
        Ok(slots)
    }

    /// A cancelled request leaves this connection unusable. Reconnect before requesting again.
    pub async fn request(&mut self, body: Body) -> Result<Body> {
        if matches!(&body, Body::ListHosts(_))
            && !self
                .session()
                .capabilities
                .iter()
                .any(|capability| capability == LIST_HOSTS_CAPABILITY)
        {
            return Err(ProtocolFault::new(
                "UnsupportedCapability",
                "companion does not support host listing",
            )
            .into());
        }
        let request = self
            .session()
            .envelope(Some(Uuid::new_v4().to_string()), body);
        self.session().validate_request(&request)?;
        let expected = request.request_id.clone();
        use corvid_client::endpoint_time::timeout;
        let reply = timeout(CALL_TIMEOUT + COMMAND_TIMEOUT, self.0.exchange(request))
            .await
            .context("companion result timed out")??;
        self.session().validate_scope(&reply)?;
        ensure!(
            reply.request_id == expected
                && reply.sequence.is_none()
                && reply.subscription_id.is_none(),
            "invalid companion response scope"
        );
        match reply.body.ok_or_else(|| {
            ProtocolFault::new("UnsupportedCapability", "unknown companion response body")
        })? {
            Body::Error(fault) => Err(ProtocolFault::from_wire(&fault).into()),
            body => Ok(body),
        }
    }
}

fn presentation_absent(error: &anyhow::Error) -> bool {
    use std::io::{Error, ErrorKind};

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if error.downcast_ref::<nix::errno::Errno>() == Some(&nix::errno::Errno::ENOENT) {
        return true;
    }
    error.downcast_ref::<Error>().is_some_and(|error| {
        error.kind() == ErrorKind::NotFound
            || (cfg!(any(target_os = "linux", target_os = "macos"))
                && error.kind() == ErrorKind::ConnectionRefused)
    })
}
