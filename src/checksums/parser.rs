//! Parser for checksums.yaml file format

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Root structure of checksums.yaml
///
/// The actual ACFS format is:
/// ```yaml
/// installers:
///   tool_name:
///     url: "https://..."
///     sha256: "hex..."
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecksumsFile {
    /// Map of installer name to entry, nested under the `installers` key
    #[serde(default)]
    pub installers: HashMap<String, InstallerEntry>,
}

/// Entry for a single installer/tool in checksums.yaml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallerEntry {
    /// Download URL
    pub url: Option<String>,
    /// Expected SHA-256 hash
    pub sha256: Option<String>,
    /// Tool version (not present in current ACFS format, kept for forward compat)
    #[serde(default)]
    pub version: Option<String>,
    /// Whether the installer is enabled (defaults to true)
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Tags for categorization
    #[serde(default)]
    pub tags: Vec<String>,
    /// Absorb unknown fields gracefully
    #[serde(flatten)]
    pub extra: HashMap<String, serde_yaml::Value>,
}

fn default_enabled() -> bool {
    true
}

/// Parse a checksums.yaml file
pub fn parse_checksums(path: &Path) -> Result<ChecksumsFile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read checksums file: {}", path.display()))?;

    let checksums: ChecksumsFile = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse checksums YAML: {}", path.display()))?;

    Ok(checksums)
}

/// Get list of enabled installers
pub fn get_enabled_installers(checksums: &ChecksumsFile) -> Vec<(&String, &InstallerEntry)> {
    checksums.installers.iter().filter(|(_, entry)| entry.enabled).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_simple_checksums() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
installers:
  rust:
    url: "https://sh.rustup.rs"
    sha256: "abc123"
  nodejs:
    url: "https://nodejs.org/dist/v20.10.0/node-v20.10.0-linux-x64.tar.xz"
    sha256: "def456"
    enabled: false
"#
        )
        .unwrap();

        let checksums = parse_checksums(file.path()).unwrap();
        assert!(checksums.installers.contains_key("rust"));
        assert!(checksums.installers.contains_key("nodejs"));

        let rust = &checksums.installers["rust"];
        assert!(rust.enabled); // default true
        assert_eq!(rust.url, Some("https://sh.rustup.rs".to_string()));
        assert_eq!(rust.sha256, Some("abc123".to_string()));

        let nodejs = &checksums.installers["nodejs"];
        assert!(!nodejs.enabled);
    }
}
