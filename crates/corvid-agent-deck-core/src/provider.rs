use corvid_protocol::wire::AgentProvider;

/// Returns the canonical display name for a known provider wire value.
pub fn known_display_name(raw: i32) -> Option<&'static str> {
    match AgentProvider::try_from(raw).ok()? {
        AgentProvider::Claude => Some("Claude"),
        AgentProvider::Codex => Some("Codex"),
        AgentProvider::Antigravity => Some("Antigravity"),
        AgentProvider::Unspecified => None,
    }
}

/// Returns the display name for a provider wire value, falling back to `"Agent"`.
pub fn display_name(raw: i32) -> &'static str {
    known_display_name(raw).unwrap_or("Agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_all_agent_providers_and_unknown_wire_values() {
        assert_eq!(
            known_display_name(AgentProvider::Claude as i32),
            Some("Claude")
        );
        assert_eq!(
            known_display_name(AgentProvider::Codex as i32),
            Some("Codex")
        );
        assert_eq!(
            known_display_name(AgentProvider::Antigravity as i32),
            Some("Antigravity")
        );
        assert_eq!(known_display_name(AgentProvider::Unspecified as i32), None);
        assert_eq!(known_display_name(99), None);

        assert_eq!(display_name(AgentProvider::Claude as i32), "Claude");
        assert_eq!(display_name(AgentProvider::Codex as i32), "Codex");
        assert_eq!(
            display_name(AgentProvider::Antigravity as i32),
            "Antigravity"
        );
        assert_eq!(display_name(AgentProvider::Unspecified as i32), "Agent");
        assert_eq!(display_name(99), "Agent");
    }
}
