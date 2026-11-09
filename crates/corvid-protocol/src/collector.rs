pub const CAPABILITY: &str = "collector.v1";
pub const INPUT_BATCH_CAPABILITY: &str = "collector.input_batch.v1";
pub const NESTED_SUBAGENT_CAPABILITY: &str = "collector.nested_subagent.v1";
pub const SHELL_AGENT_CAPABILITY: &str = "collector.shell_agent.v1";
// Leave room for 256-byte IDs and the envelope in the 8 KiB collector frame.
pub const MAX_BATCH_INPUTS: usize = 24;
pub const LAUNCH_ASSOCIATION: &str = "CORVID_LAUNCH_ASSOCIATION";
pub const HOST_ID: &str = "CORVID_HOST_ID";
pub const SERVER_EPOCH: &str = "CORVID_SERVER_EPOCH";
pub const RUNTIME_DIR: &str = "CORVID_RUNTIME_DIR";
pub const SHELL_TERMINAL: &str = "CORVID_SHELL_TERMINAL";

pub const CLAUDE_VERSION: &str = "2.1.318";
pub const CODEX_VERSION: &str = "0.167.0";
pub const CODEX_LAUNCH_CAPABILITY: &str = "agent_launch.codex.v1";
