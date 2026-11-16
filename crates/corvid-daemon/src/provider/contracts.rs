use corvid_core::agent::Provider;
use corvid_protocol::collector::{CLAUDE_VERSION, CODEX_VERSION};
use semver::Version;

/// Version-independent admission metadata for a provider's existing hook contract.
pub(crate) struct Contract {
    pub(crate) provider: Provider,
    pub(crate) program: &'static str,
    pub(crate) fixture_version: &'static str,
    pub(crate) schema_revision: u32,
    version_prefix: &'static str,
    version_suffix: &'static str,
}

pub(crate) const CONTRACTS: &[Contract] = &[
    Contract {
        provider: Provider::Claude,
        program: "claude",
        fixture_version: CLAUDE_VERSION,
        schema_revision: 3,
        version_prefix: "",
        version_suffix: " (Claude Code)",
    },
    Contract {
        provider: Provider::Codex,
        program: "codex",
        fixture_version: CODEX_VERSION,
        schema_revision: 1,
        version_prefix: "codex-cli ",
        version_suffix: "",
    },
];

pub(crate) fn contract(provider: Provider) -> &'static Contract {
    CONTRACTS
        .iter()
        .find(|contract| contract.provider == provider)
        .expect("registered provider contract")
}

impl Contract {
    pub(crate) fn recognizes_version(&self, output: &str) -> bool {
        // A fixture release is evidence about the adapter, not an admission range.
        // Every provider still validates actual hook fields and process ownership.
        output
            .trim()
            .strip_prefix(self.version_prefix)
            .and_then(|version| version.strip_suffix(self.version_suffix))
            .is_some_and(|version| Version::parse(version).is_ok())
    }

    pub(crate) fn evidence_version(&self) -> String {
        format!("{}-hooks.v{}", self.program, self.schema_revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_older_newer_and_prerelease_providers_without_a_version_range() {
        for contract in CONTRACTS {
            for version in [
                "0.1.0",
                "1.0.0",
                "2.1.300",
                "2.1.325",
                "0.166.0",
                "0.168.0-alpha.1",
                "3.0.0",
                contract.fixture_version,
            ] {
                let banner = format!(
                    "{}{version}{}\n",
                    contract.version_prefix, contract.version_suffix
                );
                assert!(contract.recognizes_version(&banner), "{banner}");
            }
        }
    }

    #[test]
    fn rejects_malformed_and_other_provider_version_banners() {
        for contract in CONTRACTS {
            for version in ["2.1", "02.1.325", "garbage", "2.1.325\nunexpected", ""] {
                let banner = format!(
                    "{}{version}{}",
                    contract.version_prefix, contract.version_suffix
                );
                assert!(!contract.recognizes_version(&banner), "{banner}");
            }
            for other in CONTRACTS {
                if other.provider == contract.provider {
                    continue;
                }
                let banner = format!(
                    "{}{}{}",
                    other.version_prefix, other.fixture_version, other.version_suffix
                );
                assert!(!contract.recognizes_version(&banner), "{banner}");
            }
        }
    }
}
