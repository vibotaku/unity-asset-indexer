//! `uai`: index a library of `.unitypackage` files, search it, resolve dependency closures, preview
//! assets and extract exactly what a Unity project needs. See the README for the big picture.

pub mod client;
pub mod config;
pub mod db;
pub mod deps;
pub mod error;
pub mod exporter;
pub mod indexer;
pub mod mcp;
pub mod model;
pub mod resolve;
pub mod server;
pub mod service;
pub mod unitypackage;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Human-readable byte count (`1.5MB`).
pub fn human_bytes(n: i64) -> String {
    let mut v = n.max(0) as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if v < 1024.0 || unit == "TB" {
            return if unit == "B" { format!("{v:.0}{unit}") } else { format!("{v:.1}{unit}") };
        }
        v /= 1024.0;
    }
    format!("{v:.1}TB")
}
