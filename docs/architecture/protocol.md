# IPC, Protocol, and Snapshot Consistency

> Split from the Corvid implementation design. See [Architecture Overview](overview.md) for context.

## 8. IPC and external control contract

### 8.1 Discovery, authentication, and SSH

Proposed paths (directories 0700, files/sockets 0600, owner is the daemon UID):

```text
$XDG_RUNTIME_DIR/corvid/client.sock
$XDG_RUNTIME_DIR/corvid/control.sock
$XDG_RUNTIME_DIR/corvid/collector.sock
$XDG_STATE_HOME/corvid/identity.json
$XDG_STATE_HOME/corvid/state.json
```

Use a validated private runtime directory, rejecting unexpected owners, writable parent
components, and symlinks. If XDG_RUNTIME_DIR is unavailable, require an explicitly
configured secure directory; do not silently use a predictable world-writable `/tmp`
path. A per-user instance lock prevents concurrent daemon creation. A stale socket is
removed only after ownership/type/lock checks and a failed liveness handshake; never
unlink a live incompatible daemon's socket.

On Linux, verify connecting UID using `SO_PEERCRED` on stream sockets as well as file
permissions. The client verifies daemon ownership too. This mechanism is documented in
[unix(7)](https://man7.org/linux/man-pages/man7/unix.7.html), checked 2026-11-07.
Separate socket roles constrain accidental misuse: only the TUI role can submit input;
control has no input/spawn/approve methods; collector accepts only observations. This
is a single-OS-user trust boundary, not isolation from malicious code already running
as that same user. A launch token is correlation data, not a substitute for authentication.

The companion connects through the remote control relay.

```text
local companion -> separate background SSH stdio -> remote control relay
                -> control.sock -> daemon

first attempt: ssh -T <configured-host-alias> corvid control --stdio
```

`-T` disables PTY allocation; see [OpenSSH ssh(1)](https://man.openbsd.org/ssh.1), checked
2026-11-07. The relay only bridges the control protocol. It neither auto-starts a missing daemon
nor opens an agent/terminal; return `DaemonUnavailable` until the user starts/attaches.
The control SSH channel can reconnect independently of the working SSH terminal.
If a saved SSH alias cannot reach the daemon with the direct command, the
companion can detect the remote platform and retry with the same remote launcher
used by Desktop. The launcher resolves a PATH or Desktop-managed installation
and uses the platform's runtime directory rules.

An explicit Desktop connection may prepare the daemon before opening its
writer and control relays. It probes through SSH, starts an installed binary
when needed, and can transfer a packaged binary matching the remote OS and CPU
to the SSH user's private installation directory. Linux and Windows support
x64 and ARM64; macOS supports Apple Silicon. The client verifies the bundle locally and
the remote installer verifies size and SHA-256 before publishing the file
without replacement. The startup lock serializes daemon startup.

Use the user's SSH configuration, known_hosts, keys, and authentication agent. Preserve
host-key checking; changed or unknown keys require the normal user trust workflow.
Background reconnection uses noninteractive authentication and reports `AuthenticationRequired`
instead of looping password prompts.
Do not automatically enable SSH agent forwarding. Bound each OpenSSH connection
attempt to 10 seconds. Automatically reconnect with jittered backoff for up to
60 seconds after detecting an outage, then wait for the user to reconnect.

Invoke the local SSH executable with structured arguments and an explicitly configured
host alias (reject aliases beginning with `-`). The remote command is fixed, since SSH
may pass a command through a remote shell. Send all variable data over stdin as framed
Protobuf; never insert workspace names, titles, questions, or IDs into the command string.
Protocol stdout contains only the binary preamble and frames; diagnostics go to bounded
stderr. Unexpected shell startup output is a protocol error, not a control message.
The relay forwards bounded frames without decoding/re-encoding their Protobuf payloads,
so it does not discard fields introduced by another compatible endpoint version.
The transport remains Unix stream sockets locally and ordinary SSH stdio remotely.
Protobuf does not introduce a gRPC/HTTP server or a direct UDP/QUIC endpoint.

### 8.2 Public protocol v1

Use **Protobuf binary messages** for the public control API and internal structured IPC.
Define a shared `proto3` schema package, proposed as `corvid.protocol.v1`, and generate
wire types independently of Rust domain types. Application protocol, provider adapter,
serialization library, code generator, and storage schema versions remain independent.
The reason for this choice is an explicit shared contract and controlled schema evolution;
no throughput or latency improvement has been measured.

An Envelope carries `protocol_major`, `protocol_minor`, optional `request_id`, `host_id`,
`server_epoch`, and a typed `oneof body`. The body identifies the method/result/event;
there is no string-dispatched command or arbitrary serialized Rust enum. Request IDs are
RpcRequestIds, not input RequestIds. Responses echo them; unsolicited events omit them
and include subscription ID and sequence. Only initial Hello and errors before a
successful handshake may omit host/epoch; such errors never update cached host state.

Use opaque bounded strings for IDs and `uint64` for sequences and revisions.
Human-readable JSON exports render those counters as decimal strings.
Use explicit presence wherever absent
and zero/empty have different meanings; enforce required semantic fields after decoding.

The [shared schema](../../crates/corvid-protocol/proto/corvid.proto) covers the handshake
and terminal transport. The excerpt shows their fields and the FocusAgent request.

```proto
syntax = "proto3";
package corvid.protocol.v1;

message Envelope {
  uint32 protocol_major = 1;
  uint32 protocol_minor = 2;
  optional string request_id = 3;
  optional string host_id = 4;
  optional string server_epoch = 5;
  optional string subscription_id = 6;
  optional uint64 sequence = 7;
  oneof body {
    Hello hello = 10;
    HelloResult hello_result = 11;
    FocusAgentRequest focus_agent = 20;
    RpcError error = 90;
  }
}
message Hello {
  optional string controller_id = 1;
  repeated uint32 supported_major_versions = 2;
  repeated string required_capabilities = 3;
}
message HelloResult {
  repeated string capabilities = 1;
  Limits limits = 2;
  string writer_policy = 3;
}
message Limits {
  uint32 control_frame_bytes = 1;
  uint32 command_timeout_ms = 2;
}
message FocusAgentRequest {
  string controller_id = 1;
  string client_id = 2;
  string agent_run_id = 3;
  optional uint64 binding_revision = 4;
}
message RpcError {
  string code = 1;
  string message = 2;
  bool retryable = 3;
}
```

The following are **decoded TextFormat illustrations**, not text sent on stdin/stdout:

```textproto
protocol_major: 1
protocol_minor: 0
request_id: "rpc-hello-1"
hello {
  controller_id: "controller-1"
  supported_major_versions: 1
  required_capabilities: "snapshot_watch.v1"
  required_capabilities: "focus_agent.v1"
}
```

```textproto
protocol_major: 1
protocol_minor: 0
request_id: "rpc-hello-1"
host_id: "host-1"
server_epoch: "epoch-7"
hello_result {
  capabilities: "list.v1"
  capabilities: "snapshot_watch.v1"
  capabilities: "focus_agent.v1"
  capabilities: "attention_ack.v1"
  capabilities: "pair_client.v1"
  limits { control_frame_bytes: 262144 command_timeout_ms: 5000 }
  writer_policy: "exclusive_or_reject"
}
```

Control and TUI HelloResult messages include the daemon build's exact release profiles
with adapter revisions, qualified events, correlation fields, and capability levels.
GetCapabilities returns the same description for a control connection. Managed launches
check the executable version and report collection state per run. Socket roles still
restrict operations. A control client identifies its ControllerId during Hello.
Peer authentication follows §8.1.

| Method | Contract |
|---|---|
| `GetHost` | Stable HostId and current ServerEpoch |
| `GetCapabilities` | Negotiated control features, limits, writer policy, and release-qualified provider profiles |
| `GetConnectionStatus` | Live terminal and persistence status, current top-level run coverage, and collector traffic for this epoch. Clients track transport status separately. |
| `ListAgents`, `ListClients`, `ListWorkspaces` | Host-scoped inventories from one revision, with current locations/evidence; no terminal text |
| `SnapshotWatch` | Atomic snapshot plus changes after its barrier (§9) |
| `GetSnapshot` | Consistent one-shot snapshot; must not be followed by a separate unguarded Subscribe |
| `FocusAgent` | Exact existing-client selection under explicit registration (§10) |
| `AcknowledgeAttention` | Idempotent acknowledgment by AttentionId, scoped to the named run/epoch |
| `RegisterClientBinding`, `UnregisterClientBinding` | Explicit single-use pairing or removal of one controller/client association; no input rights |
| `Ping` | Link health only |
| Companion `ListHosts` | Configured endpoints and their remote identities/status; one daemon does not enumerate other hosts |

Snapshots include host/epoch/sequence, workspace trees, terminals and run-location
mappings, current/historical run observations, requests/attention metadata, clients and
lease ownership, relevant binding revisions, and capabilities/collection health.
They exclude PTY bytes, prompts, answer contents, credentials, and pairing secrets.
Default titles are sanitized and bounded; question kind/summary labels suffice without
exporting the full question text.

Errors have `code`, safe `message`, `retryable`, and optional structured `details`.
Use stable codes: `ProtocolMismatch`, `UnsupportedCapability`, `InvalidMessage`,
`MessageTooLarge`, `Unauthorized`, `HostMismatch`, `EpochMismatch`, `DaemonUnavailable`,
`NoAttachedClient`, `AmbiguousClient`, `TargetGone`, `StaleBinding`, `WriterRequired`,
`WorkspaceBusy`, `InputBusy`, `ResyncRequired`, `LimitExceeded`, `Timeout`.
Companion-local errors include `LocalTerminalUnavailable` and `ConnectionRequired`.
An error never triggers an automatic replacement target or interactive connection.

Limits: 256 KiB/control frame including its kind byte, message nesting depth 32,
256 UTF-8 bytes/ID, 1 KiB/display string, and 32 concurrent commands/connection.
Check frame lengths before allocation, configure bounded decoder recursion, and validate
collection counts, strings, and semantic presence before any state mutation. Wire size
alone does not bound decoded allocations; repeated/map fields also obey domain limits.
Malformed Protobuf, truncated frames, or invalid string encoding closes that connection
after a bounded error where possible. Unknown body/enum handling is specified in §8.4.

Command/handshake deadlines are 5 seconds. SnapshotWatch setup follows that deadline;
its subsequent snapshot transfer has the separate §9 limit. Ping every 10 seconds;
30 seconds without any valid inbound frame marks the control link Offline and starts
reconnection, without changing agent liveness.

Negotiate major/minor and capabilities before Attach or any mutation. Same major permits
only documented additive minor changes. A new CLI connecting to an old live daemon may
negotiate a compatible subset or return ProtocolMismatch. It must never kill/restart the
daemon or corrupt work to force an upgrade. The error includes supported versions and
manual upgrade guidance. Installation equality does not imply running-version equality.

Desktop uses the version reported by the running daemon to detect an older local
daemon before attaching. Automatic replacement requires authenticated live
terminal handoff for the same host and epoch. Desktop may fall back to a
compatible attachment when handoff is unavailable.

Daemon replacement uses an authenticated local upgrade socket, separate from
the control relay. An automatically started daemon advertises
`daemon_upgrade.v1` there. It pauses TUI admission, prepares terminal handoff
when supported, and saves metadata before approving shutdown. On older daemons
without terminal handoff, an explicit `corvid upgrade` may use the idle-only
upgrade path. The launcher serializes replacement under the startup lock,
reopens the same state directory, and verifies the same host in a new epoch.
Externally managed daemons are upgraded through their supervisor.

Cache results for up to 4,096 `(ControllerId, RpcRequestId)` commands for 10 minutes per
epoch. Reusing an ID with a different validated semantic command is InvalidMessage. Compare
normalized command fields, not encoded byte equality: wire ordering is not command
identity. Focus/ack retries within that window return the original result, not a new action.
If the client loses a response, resynchronize before deciding whether a fresh explicit action is needed; never persist
and replay click commands after reconnect. A timeout may mean the action committed;
report that uncertainty and check current view state.

### 8.3 Framing and internal TUI protocol

Use a small fixed framing layer over ordered byte streams. Each direction starts with
the four ASCII bytes `CRV1`, denoting framing version 1, then frames of this form:

```text
u32_be frame_length | u8 kind | payload

kind 1: Protobuf Envelope
kind 2: u32_be header_length | Protobuf RenderHeader | raw ANSI bytes
```

`frame_length` includes the kind and payload, excluding its own four bytes. Zero length,
unknown preamble/kind, overflow, truncation, and frames forbidden on the socket role
are errors. Read exactly the stated byte count; neither a socket read nor a Protobuf
field boundary is a frame boundary. Never scan arbitrary bytes for the next magic value
to recover a corrupt stream. Close and re-handshake instead.

Kind 1 is limited to 256 KiB. Kind 2 is permitted only on the daemon-to-TUI path and is
limited to 4 MiB, including kind, header length, header, and ANSI bytes; its header is
at most 16 KiB and must fit inside the outer frame. Check the role-specific length limit
before allocating the payload. RenderHeader contains HostId, ServerEpoch, ClientId, view
revision, render generation, frame ID, optional base-frame ID (absent for a full redraw),
and ANSI byte length. Check the inner/outer lengths agree before emitting any ANSI.
Apply the §7 redraw, generation, and target-validation rules before emitting a frame.

Control, collector, and companion-presentation streams use kind 1 only. Each has a
role-specific set of body types and operations. The collector has an 8 KiB limit
and accepts one observation per authenticated connection. Use ordinary Protobuf
encode/decode within a frame, without adding a
second codec-specific length-delimited wrapper. A future framing change needs separate
negotiation; the fixed bootstrap Hello/Error subset must remain readable before application
version agreement. Unsupported framing fails locally as ProtocolMismatch without touching
the daemon's running work.

Protobuf does not delimit consecutive messages by itself; the length-prefix choice follows
its [streaming guidance](https://protobuf.dev/programming-guides/techniques/#streaming-multiple-messages),
checked 2026-11-07. Do not encode an entire 16 MiB snapshot as one Protobuf message; send
bounded chunks. Keep bulk rendering as raw bytes with a small typed header.

Illustrative message families, not serialized Rust enum layouts:

```rust
// TUI -> daemon, all after negotiated Hello:
Attach { workspace_id, viewport, terminal_capabilities, requested_role }
InputChunk { route_token, input_sequence, bytes }
PasteBegin { route_token, paste_id }
PasteChunk { paste_id, bytes }
PasteEnd { paste_id }
ResizeViewport { client_id, connection_generation, viewport }
SelectView { expected_view_revision, workspace_id, tab_id, pane_id }
ClaimWorkspaceWriter { workspace_id } // explicit user action, never via control socket
FrameApplied { render_generation, frame_id }
Detach

// daemon -> TUI:
Attached { client_id, connection_generation, route_token, writer_leases }
ViewChanged { view_revision, selection, route_token }
RenderFrame { header, ansi_bytes }
AttentionChanged { metadata }
ProtocolError { code, supported_versions }
```

InputChunk and PasteChunk use Protobuf `bytes` fields without base64. Each envelope
fits the control-frame limit; a paste may span several chunks while retaining the §7.4
transaction/target rules. Separate per-client I/O tasks and queues prevent a blocked TUI
from blocking control replies. External watches never carry RenderFrame. EOF or protocol
error cleans up only that connection and its input leases.

The TUI Hello advertises `adaptive_viewport.v1` when the daemon accepts outer dimensions
beyond the legacy 300×120 bounds. Clients report an upgrade requirement before sending
an oversized Attach or ResizeViewport to a legacy daemon.

### 8.4 Schema evolution and code generation

Protobuf parsing compatibility is not semantic compatibility. Keep the handshake,
capability checks, state validation, and explicit errors even when decoding succeeds.

- Assign stable field/enum numbers. Reserve removed numbers and names; never reuse them,
  change an existing field's meaning/type, or move fields between existing oneofs as an
  assumed safe minor update. Prefer additive optional fields with defined absent behavior.
- Unknown optional fields may be skipped at an endpoint. A request with no recognized
  body returns UnsupportedCapability; it never becomes a default command. An unrecognized
  result or state-changing event invalidates that consumer's pending result/cache and
  requires compatible resync/upgrade, rather than silently advancing its sequence.
- Give enums an unspecified zero value. Validate raw numeric values, including unknown
  ones, before deriving state or dispatching actions; never map an unknown value to a
  reassuring default. `prost` enum getters can return a default for unrecognized values,
  so domain conversion must explicitly validate them.
- Validate required application fields, ID scopes, a nonzero protocol major, and presence
  of required preconditions such as binding_revision. Missing does not silently mean
  revision zero; minor version zero is valid.
- Preserve opaque payloads in the SSH relay. Do not rely on a particular generated
  runtime preserving unknown fields after decode/re-encode, or convert live traffic to
  JSON and back. No schema download, runtime reflection, or generic Any dispatch is needed.

Field evolution/presence rules are grounded in the official
[proto3 guide](https://protobuf.dev/programming-guides/proto3/#updating) and the Rust
[prost API](https://docs.rs/prost/0.14.4/prost/), checked 2026-11-07. The handling policies
above are Corvid decisions and require version-skew fixtures, not just successful decoding.

For implementation, keep `.proto` definitions in one shared protocol package; daemon,
TUI, and Rust companion consume generated wire types and explicit domain conversions.
The protocol crate uses `protox` 0.10.0 and `prost-build` 0.14.4. Cargo uses
the same schema descriptor to generate wire types and field tables for bounded
decode preflight. The reproducibility fixture compares both outputs against
the pinned `protoc` 36.1 reference. The preflight
rejects legacy groups, limits known message depth to 32, and bounds repeated fields
before decoding. Apply method-specific semantic limits before executing requests.
Generate into the build output directory and leave generated Rust untouched.
The [protox documentation](https://docs.rs/protox/0.10.0/protox/) describes
descriptor generation for Prost.

JSON remains for schema-versioned persistence/configuration, provider-defined payloads,
and optional human-readable CLI/debug output. A proposed `--json` diagnostic mode decodes
validated messages locally, redacts sensitive fields, and prints text; it is not another
negotiated live wire protocol. Do not serialize generated wire structs directly as the
storage schema.

## 9. Snapshot and watch consistency

The daemon serves `GetSnapshot`, `SnapshotWatch`, and `ListClients` through the
control socket and the stdio relay. Snapshot entities include the layout tree,
live terminal locations, client views and writer leases, agent evidence, requests,
and attention. The negotiated capabilities are included in `SnapshotBegin`.
The daemon sends changed entities and explicit removals in each transaction delta.
The Rust snapshot assembler validates the layout, entity references, scope, and
watermark before exposing a cache. Live transactions validate as a unit and a
failed transaction leaves the previous cache intact.

### 9.1 Atomic barrier

Use a monotonic `u64` public sequence within each ServerEpoch. Initial sequence is zero;
if exhausted, refuse further publication and require controlled maintenance rather than
wrap. v1 reconnect always obtains a new complete snapshot. There is no durable event log,
cross-host order, or distributed consensus.

`SnapshotWatch` is one coordinator operation:

1. At a transaction boundary, capture an immutable domain snapshot at S and install the
   subscriber's bounded delta queue in the same critical section.
2. Release the coordinator; encode/stream that immutable snapshot outside the reducer.
3. Send SnapshotBegin(S), indexed chunks, and SnapshotEnd(S). Maximum total snapshot is
   16 MiB and transfer deadline 30 seconds; each Protobuf chunk/envelope obeys the
   256 KiB control-frame limit.
4. Send queued deltas S+1 onward in order, then a WatchReady watermark W after all deltas
   through W have been sent. Subsequent deltas continue normally.
5. If a queue fills during transfer, abort with ResyncRequired/disconnect. Never label
   an incomplete snapshot ready. Do not hold PTY/reducer progress for slow encoding/writing.

The client builds a staging cache, checks epoch, chunk completeness, entity references,
and contiguous deltas through W, then atomically replaces its visible cache and marks
Ready. A one-shot snapshot becomes current only at S, not a promise that no later changes
exist. Phase 1 can use bounded full copies; copy-on-write is unnecessary before measurement.

### 9.2 Race example

```text
companion -> daemon: SnapshotWatch(request_id=q1)
coordinator: capture snapshot S=40; register watch queue atomically
encoder -> companion: SnapshotBegin(epoch=E, seq=40)
coordinator: A asks question R; commit seq=41; enqueue delta(41)
encoder -> companion: SnapshotChunk(0...n), all from immutable state at 40
coordinator: A moves panes; commit seq=42; enqueue delta(42)
encoder -> companion: SnapshotEnd(seq=40)
watch -> companion: Delta(41, R pending + attention), Delta(42, location mapping)
watch -> companion: WatchReady(through_seq=42)
companion: swap staged cache; A shows R at its new location; mark Ready
```

There is no gap between “read snapshot” and “subscribe.” If 42 arrives twice, ignore the
duplicate after applying it once. If 44 arrives when 43 is expected, stop applying,
mark Syncing, disable navigation, and request a full snapshot. Any epoch change discards
staged data and invalidates live references. Events with an old epoch cannot update a new
cache. v1 watches are unfiltered host feeds for this single user; adding filtered feeds
later must define sequence watermarks so filtering is not mistaken for loss.

### 9.3 Reconnection is not revalidation

During Connecting/Syncing/Offline, cached cards show last-known time and uncertainty.
Keep slots, pins, and unresolved attention after a socket closes.
The remote collector continues while the PC is off, so a question observed during
that time appears in the next snapshot if the daemon remained healthy.

Reading the collector's snapshot reestablishes cache consistency, not missing agent
truth. If collection lost events, invalidate the affected claims and attempt only the
adapter's supported read-only revalidation. Without such support, use Unknown/Unverified
and ask the user to inspect the existing terminal. A current process probe cannot repair
missing question/turn history. A healthy daemon heartbeat cannot repair it either.
