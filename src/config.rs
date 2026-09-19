//! Settings: where the library is, where the index lives, and (optionally) which server to talk to.
//!
//! Precedence: explicit flags > environment (`UAI_LIBRARY`, `UAI_HOME`, `UAI_SERVER`) > `config.json`
//! in the home directory > defaults.

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub library: PathBuf,
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
    pub fn library_mounted(&self) -> bool {
        !self.library.as_os_str().is_empty() && self.library.is_dir()
    }

    fn read_file(path: &Path) -> ConfigFile {
        fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn write_file(path: &Path, data: &ConfigFile) -> Result<()> {
        let s = serde_json::to_string_pretty(data)? + "\n";
        fs::write(path, s).with_context(|| format!("writing {}", path.display()))
    }

    /// Resolve settings. `library`, `home` and `server` are explicit overrides (flags).
    pub fn load(library: Option<&str>, home: Option<&str>, server: Option<&str>) -> Result<Config> {
        let home_dir = home
            .map(|s| s.to_string())
            .or_else(|| std::env::var("UAI_HOME").ok().filter(|s| !s.is_empty()))
            .map(|s| expand_tilde(&s))
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".unity-asset-index"));
        fs::create_dir_all(&home_dir).with_context(|| format!("creating {}", home_dir.display()))?;
        let cfg_file = home_dir.join("config.json");
        let data = if cfg_file.exists() { Self::read_file(&cfg_file) } else { ConfigFile::default() };
        let lib = library
            .map(|s| s.to_string())
            .or_else(|| std::env::var("UAI_LIBRARY").ok().filter(|s| !s.is_empty()))
            .or_else(|| data.library.clone().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| DEFAULT_LIBRARY.to_string());
        let server = server
            .map(|s| s.to_string())
            .or_else(|| std::env::var("UAI_SERVER").ok())
            .or_else(|| data.server.clone())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().trim_end_matches('/').to_string());
        if !cfg_file.exists() {
            let _ = Self::write_file(&cfg_file, &ConfigFile { library: Some(lib.clone()), server: None });
        }
        Ok(Config { library: expand_tilde(&lib), home: home_dir, server })
    }

    pub fn save_library(&mut self, library: &str) -> Result<()> {
        let mut data = Self::read_file(&self.config_file());
        data.library = Some(library.to_string());
        Self::write_file(&self.config_file(), &data)?;
        self.library = expand_tilde(library);
        Ok(())
    }

    pub fn save_server(&mut self, server: Option<&str>) -> Result<()> {
        let mut data = Self::read_file(&self.config_file());
        data.server = server.map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty());
        Self::write_file(&self.config_file(), &data)?;
        self.server = data.server;
        Ok(())
    }
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
