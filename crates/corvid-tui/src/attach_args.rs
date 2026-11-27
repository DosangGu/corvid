use std::ffi::OsString;

use clap::{Args, ValueEnum};
use corvid_client::AttachOptions;
use corvid_protocol::wire::AgentProvider;

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum ManagedAgent {
    Claude,
    Codex,
    #[value(alias = "agy")]
    Antigravity,
}

impl ManagedAgent {
    pub(crate) fn into_provider(self) -> AgentProvider {
        match self {
            Self::Claude => AgentProvider::Claude,
            Self::Codex => AgentProvider::Codex,
            Self::Antigravity => AgentProvider::Antigravity,
        }
    }
}

#[derive(Args)]
pub(crate) struct CommonAttachArgs {
    /// Start a new terminal with the user's shell.
    #[arg(long)]
    pub(crate) new: bool,
    /// Take the writer lease on an existing pane and disconnect its current writer.
    #[arg(long = "takeover", conflicts_with_all = ["new", "agent", "new_workspace", "new_tab", "argv"])]
    pub(crate) take_over: bool,
    /// Start the named provider; arguments after -- go to its CLI.
    #[arg(long, value_enum)]
    pub(crate) agent: Option<ManagedAgent>,
    /// Attach to a workspace by ID.
    #[arg(long, conflicts_with = "new_workspace")]
    pub(crate) workspace: Option<String>,
    /// Attach to a tab by ID within the workspace.
    #[arg(long, conflicts_with_all = ["new_workspace", "new_tab"])]
    pub(crate) tab: Option<String>,
    /// Attach to an exact pane within the selected workspace and tab.
    #[arg(long, requires_all = ["workspace", "tab"])]
    pub(crate) pane: Option<String>,
    /// Create a workspace with this label and open a terminal.
    #[arg(long)]
    pub(crate) new_workspace: Option<String>,
    /// Use this folder on the execution host as the workspace default.
    #[arg(long, requires = "new_workspace")]
    pub(crate) workspace_directory: Option<String>,
    /// Browse folders on the execution host before creating a workspace.
    #[arg(
        long,
        requires = "new_workspace",
        conflicts_with = "workspace_directory"
    )]
    pub(crate) browse_directory: bool,
    /// Create a tab with this label and open a terminal.
    #[arg(long)]
    pub(crate) new_tab: Option<String>,
    /// Open an existing workspace by exact name or ID.
    #[arg(value_name = "NAME_OR_ID", conflicts_with_all = ["workspace", "new_workspace"])]
    pub(crate) target: Option<String>,
    /// Program and arguments for an explicitly started terminal.
    #[arg(last = true)]
    pub(crate) argv: Vec<OsString>,
}

impl CommonAttachArgs {
    pub(crate) fn into_attach_options(
        self,
        workspace: Option<String>,
        workspace_directory: Option<String>,
    ) -> AttachOptions {
        let launch = (self.new
            || self.agent.is_some()
            || !self.argv.is_empty()
            || self.new_workspace.is_some()
            || self.new_tab.is_some())
        .then_some(self.argv);
        AttachOptions {
            remember_view: true,
            launch,
            take_over: self.take_over,
            launch_if_empty: false,
            agent: self.agent.map(ManagedAgent::into_provider),
            workspace,
            tab: self.tab,
            pane: self.pane,
            new_workspace: self.new_workspace,
            workspace_directory,
            new_tab: self.new_tab,
            ..AttachOptions::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use clap::Parser;
    use corvid_protocol::wire::AgentProvider;

    use super::CommonAttachArgs;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        attach: CommonAttachArgs,
    }

    #[test]
    fn pane_requires_workspace_and_tab() {
        assert!(TestCli::try_parse_from(["corvid", "--pane", "p1"]).is_err());
        assert!(TestCli::try_parse_from(["corvid", "--pane", "p1", "--workspace", "w1"]).is_err());
    }

    #[test]
    fn tab_conflicts_with_new_workspace() {
        assert!(
            TestCli::try_parse_from(["corvid", "--tab", "t1", "--new-workspace", "foo"]).is_err()
        );
    }

    #[test]
    fn takeover_conflicts_with_new() {
        assert!(TestCli::try_parse_from(["corvid", "--takeover", "--new"]).is_err());
    }

    #[test]
    fn parses_agent_alias_and_argv_into_attach_options() {
        let parsed = TestCli::try_parse_from([
            "corvid",
            "--new-tab",
            "work",
            "--agent",
            "agy",
            "--",
            "echo",
            "hi",
        ])
        .expect("valid attach args");
        let options = parsed.attach.into_attach_options(None, None);
        assert_eq!(options.new_tab.as_deref(), Some("work"));
        assert_eq!(options.agent, Some(AgentProvider::Antigravity));
        assert_eq!(
            options.launch,
            Some(vec![OsString::from("echo"), OsString::from("hi")])
        );
    }
}
