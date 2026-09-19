//! Settings: where the library roots are, where the index lives, and (optionally) which server to
//! talk to.
//!
//! Precedence: explicit flags > environment (`UAI_LIBRARY`, `UAI_HOME`, `UAI_SERVER`) > `config.json`
//! in the home directory > defaults. `UAI_LIBRARY` and `--library` may list several roots
//! (`--library` repeated, or the platform path-list separator in the variable).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
pub const DEFAULT_LIBRARY: &str = "/Volumes/Shared/Game Asset LIb";
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_LIBRARY: &str = "";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    /// Legacy single root (kept readable; `libraries` wins when present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libraries: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

impl ConfigFile {
    fn roots(&self) -> Vec<String> {
        let mut out: Vec<String> = self.libraries.clone().unwrap_or_default();
        if out.is_empty() {
            out.extend(self.library.clone());
        }
        out.into_iter().filter(|s| !s.trim().is_empty()).collect()
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Library roots, in priority order. The first one is the "primary" root (used for packages
    /// indexed before roots were recorded).
    pub libraries: Vec<PathBuf>,
    pub home: PathBuf,
    /// Base URL of a remote `uai serve` instance. When set, read-only commands run against it.
    pub server: Option<String>,
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.home.join("index.db")
    }
    pub fn previews_path(&self) -> PathBuf {
        self.home.join("previews.db")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.home.join("cache")
    }
    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.json")
    }
    /// The primary root (first configured), possibly empty when nothing is configured.
    pub fn primary_library(&self) -> PathBuf {
        self.libraries.first().cloned().unwrap_or_default()
    }
    pub fn mounted_roots(&self) -> Vec<&Path> {
        self.libraries.iter().map(|p| p.as_path()).filter(|p| !p.as_os_str().is_empty() && p.is_dir()).collect()
    }
    /// True when at least one root is reachable.
    pub fn library_mounted(&self) -> bool {
        !self.mounted_roots().is_empty()
    }
    /// Display string of all roots, `;`-joined.
    pub fn libraries_display(&self) -> String {
        self.libraries.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>().join("; ")
    }

    fn read_file(path: &Path) -> ConfigFile {
        fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn write_file(path: &Path, data: &ConfigFile) -> Result<()> {
        let s = serde_json::to_string_pretty(data)? + "\n";
        fs::write(path, s).with_context(|| format!("writing {}", path.display()))
    }

    /// Resolve settings. `libraries`, `home` and `server` are explicit overrides (flags).
    pub fn load(libraries: &[String], home: Option<&str>, server: Option<&str>) -> Result<Config> {
        let home_dir = home
            .map(|s| s.to_string())
            .or_else(|| std::env::var("UAI_HOME").ok().filter(|s| !s.is_empty()))
            .map(|s| expand_tilde(&s))
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".unity-asset-index"));
        fs::create_dir_all(&home_dir).with_context(|| format!("creating {}", home_dir.display()))?;
        let cfg_file = home_dir.join("config.json");
        let data = if cfg_file.exists() { Self::read_file(&cfg_file) } else { ConfigFile::default() };
        let mut roots: Vec<String> = libraries.iter().filter(|s| !s.trim().is_empty()).cloned().collect();
        if roots.is_empty() {
            if let Some(v) = std::env::var_os("UAI_LIBRARY") {
                roots = std::env::split_paths(&v)
                    .map(|p| p.to_string_lossy().to_string())
                    .filter(|s| !s.trim().is_empty())
                    .collect();
            }
        }
        if roots.is_empty() {
            roots = data.roots();
        }
        if roots.is_empty() && !DEFAULT_LIBRARY.is_empty() {
            roots.push(DEFAULT_LIBRARY.to_string());
        }
        let server = server
            .map(|s| s.to_string())
            .or_else(|| std::env::var("UAI_SERVER").ok())
            .or_else(|| data.server.clone())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().trim_end_matches('/').to_string());
        if !cfg_file.exists() {
            let _ = Self::write_file(
                &cfg_file,
                &ConfigFile { library: None, libraries: Some(roots.clone()), server: None },
            );
        }
        Ok(Config { libraries: roots.iter().map(|s| expand_tilde(s)).collect(), home: home_dir, server })
    }

    fn save_roots(&mut self, roots: Vec<String>) -> Result<()> {
        let mut data = Self::read_file(&self.config_file());
        data.library = None;
        data.libraries = Some(roots.clone());
        Self::write_file(&self.config_file(), &data)?;
        self.libraries = roots.iter().map(|s| expand_tilde(s)).collect();
        Ok(())
    }

    /// Replace the root list with a single root.
    pub fn save_library(&mut self, library: &str) -> Result<()> {
        self.save_roots(vec![library.to_string()])
    }

    pub fn add_library(&mut self, library: &str) -> Result<()> {
        let mut roots: Vec<String> = self.libraries.iter().map(|p| p.to_string_lossy().to_string()).collect();
        let new = expand_tilde(library).to_string_lossy().to_string();
        if !roots.iter().any(|r| same_root(r, &new)) {
            roots.push(library.to_string());
        }
        self.save_roots(roots)
    }

    pub fn remove_library(&mut self, library: &str) -> Result<bool> {
        let target = expand_tilde(library).to_string_lossy().to_string();
        let before = self.libraries.len();
        let roots: Vec<String> =
            self.libraries.iter().map(|p| p.to_string_lossy().to_string()).filter(|r| !same_root(r, &target)).collect();
        self.save_roots(roots)?;
        Ok(self.libraries.len() != before)
    }

    pub fn save_server(&mut self, server: Option<&str>) -> Result<()> {
        let mut data = Self::read_file(&self.config_file());
        data.server = server.map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty());
        Self::write_file(&self.config_file(), &data)?;
        self.server = data.server;
        Ok(())
    }
}

fn same_root(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim_end_matches(['/', '\\']).to_string();
    norm(a) == norm(b)
}

pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            let rest = rest.trim_start_matches(['/', '\\']);
            return if rest.is_empty() { home } else { home.join(rest) };
        }
    }
    PathBuf::from(s)
}
