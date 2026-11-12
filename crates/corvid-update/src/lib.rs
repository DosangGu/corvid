//! GitHub release discovery and installation, independent of daemon lifecycle.
mod download;
mod install;

use anyhow::{Context, Result, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};

pub use install::{InstallProgress, Installation, Installed, launch};

pub const REPOSITORY: &str = "DosangGu/corvid";
pub const RELEASES: &str = "https://github.com/DosangGu/corvid/releases";
const MAX_METADATA: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Product {
    Tui,
    Desktop,
}

impl Product {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Desktop => "desktop",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Package {
    pub schema_version: u32,
    pub version: String,
    pub source_revision: String,
    pub product: Product,
    pub target: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub version: String,
    pub source_revision: String,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    pub notes: String,
    pub manifest: Manifest,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
}

pub fn version(input: &str) -> Result<Version> {
    let input = input.strip_prefix('v').unwrap_or(input);
    let parsed = Version::parse(input).context("invalid release version")?;
    ensure!(
        parsed.build.is_empty(),
        "release versions cannot contain build metadata"
    );
    Ok(parsed)
}

pub fn check(requested: Option<&str>) -> Result<Release> {
    let suffix = match requested {
        Some(input) => format!("tags/v{}", version(input)?),
        None => "latest".into(),
    };
    let bytes = download::bytes(
        &format!("https://api.github.com/repos/{REPOSITORY}/releases/{suffix}"),
        MAX_METADATA,
    )?;
    let release: GithubRelease =
        serde_json::from_slice(&bytes).context("reading GitHub release")?;
    ensure!(!release.draft, "the release is not published");
    let selected = version(&release.tag_name)?;
    ensure!(
        release.tag_name == format!("v{selected}"),
        "unexpected release tag"
    );
    if let Some(requested) = requested {
        ensure!(
            selected == version(requested)?,
            "GitHub returned another release"
        );
    } else {
        ensure!(
            !release.prerelease && selected.pre.is_empty(),
            "latest release is not stable"
        );
    }
    let bytes = download::bytes(
        &format!("{RELEASES}/download/v{selected}/release-manifest.json"),
        MAX_METADATA,
    )?;
    let manifest: Manifest = serde_json::from_slice(&bytes).context("reading release manifest")?;
    validate_manifest(&manifest, &selected)?;
    Ok(Release {
        version: selected,
        notes: release.body.unwrap_or_default(),
        manifest,
    })
}

fn validate_manifest(manifest: &Manifest, selected: &Version) -> Result<()> {
    ensure!(manifest.schema_version == 1, "unsupported release manifest");
    ensure!(
        manifest.version == selected.to_string(),
        "release manifest version mismatch"
    );
    ensure!(
        manifest.source_revision.len() == 40
            && manifest
                .source_revision
                .bytes()
                .all(|c| c.is_ascii_hexdigit()),
        "invalid source revision"
    );
    ensure!(
        !manifest.assets.is_empty() && manifest.assets.len() <= 32,
        "invalid release asset count"
    );
    let mut names = std::collections::HashSet::new();
    for asset in &manifest.assets {
        ensure!(
            !asset.name.is_empty()
                && asset.name.len() <= 128
                && asset
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c)),
            "invalid asset name"
        );
        ensure!(names.insert(&asset.name), "duplicate release asset");
        ensure!(
            asset.size > 0 && asset.size <= 1024 * 1024 * 1024,
            "invalid asset size"
        );
        ensure!(
            asset.sha256.len() == 64
                && asset
                    .sha256
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "invalid asset SHA-256"
        );
    }
    Ok(())
}

impl Release {
    pub fn is_newer_than(&self, current: &str) -> Result<bool> {
        Ok(self.version.cmp_precedence(&version(current)?).is_gt())
    }

    pub fn url(&self) -> String {
        format!("{RELEASES}/tag/v{}", self.version)
    }

    pub fn asset(&self, name: &str) -> Result<&Asset> {
        self.manifest
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .with_context(|| format!("release is missing {name}"))
    }
}

pub fn target(product: Product) -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH, product) {
        ("linux", "x86_64", Product::Tui) => Ok("x86_64-unknown-linux-musl"),
        ("linux", "x86_64", Product::Desktop) => Ok("x86_64-unknown-linux-gnu"),
        ("windows", "x86_64", _) => Ok("x86_64-pc-windows-msvc"),
        _ => anyhow::bail!("this platform has no Corvid release package"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            schema_version: 1,
            version: "0.2.0".into(),
            source_revision: "a".repeat(40),
            assets: vec![Asset {
                name: "install.sh".into(),
                size: 100,
                sha256: "b".repeat(64),
            }],
        }
    }

    #[test]
    fn rejects_mixed_releases_and_ambiguous_assets() {
        let selected = version("0.2.0").unwrap();
        let mut input = manifest();
        validate_manifest(&input, &selected).unwrap();
        input.version = "0.1.0".into();
        assert!(validate_manifest(&input, &selected).is_err());
        input.version = "0.2.0".into();
        input.assets.push(input.assets[0].clone());
        assert!(validate_manifest(&input, &selected).is_err());
        input.assets.pop();
        input.assets[0].name = "../install.sh".into();
        assert!(validate_manifest(&input, &selected).is_err());
    }

    #[test]
    fn compares_versions_by_semver_precedence() {
        let release = Release {
            version: version("0.10.0").unwrap(),
            notes: String::new(),
            manifest: manifest(),
        };
        assert!(release.is_newer_than("0.9.0").unwrap());
        assert!(!release.is_newer_than("0.10.0").unwrap());
        assert!(!release.is_newer_than("1.0.0").unwrap());
        assert!(version("../../other").is_err());
        assert!(version("1.0.0+local").is_err());
    }
}
