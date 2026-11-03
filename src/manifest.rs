use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::util;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub release: Option<String>,
    pub packages: Vec<ManifestPackage>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestPackage {
    #[serde(default)]
    pub name: Option<String>,
    pub pkgname: String,
    pub version: String,
    pub arch: String,
    pub filename: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    pub url: String,
    #[serde(default)]
    pub built_at: Option<String>,
    #[serde(default)]
    pub asset_id: Option<u64>,
    #[serde(default)]
    pub asset_updated_at: Option<String>,
}

pub fn load(source: &str, timeout: Duration) -> Result<Manifest> {
    let bytes = util::fetch_manifest(source, timeout)?;
    let manifest =
        serde_json::from_slice(&bytes).with_context(|| format!("解析 manifest 失败: {source}"))?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_snapshot() {
        let json = r#"{
            "generated_at": "2026-11-03T02:31:50Z",
            "release": "packages",
            "packages": [
                {
                    "name": "fnm-bin",
                    "pkgname": "fnm-bin",
                    "version": "1.39.0-1",
                    "arch": "x86_64",
                    "filename": "fnm-bin-1.39.0-1-x86_64.pkg.tar.zst",
                    "sha256": "ae34b207417e5fec55d88ee0163254ff477cf5193c4f1e2b2e17599c83b97569",
                    "size": 3225586,
                    "built_at": "2026-11-03T02:31:25Z",
                    "asset_id": 703980727,
                    "asset_updated_at": "2026-11-03T02:31:58Z",
                    "url": "https://github.com/quantulr/archlinux-repo/releases/download/packages/fnm-bin-1.39.0-1-x86_64.pkg.tar.zst"
                }
            ]
        }"#;

        let manifest: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.release.as_deref(), Some("packages"));
        assert_eq!(manifest.packages.len(), 1);
        let pkg = &manifest.packages[0];
        assert_eq!(pkg.pkgname, "fnm-bin");
        assert_eq!(pkg.version, "1.39.0-1");
        assert_eq!(pkg.arch, "x86_64");
        assert_eq!(pkg.size, Some(3_225_586));
        assert!(pkg.sha256.is_some());
    }
}
