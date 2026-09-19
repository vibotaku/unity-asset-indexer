//! Plain data types shared by the database layer, the CLI, the HTTP API and the remote client.
//! Their JSON shape is the public contract (`--json` output and `/api/*` responses).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Package {
    pub id: i64,
    /// Library root this package was found under.
    #[serde(default)]
    pub root: String,
    pub rel_path: String,
    pub name: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub mtime: f64,
    #[serde(default)]
    pub indexed_at: Option<f64>,
    #[serde(default)]
    pub entry_count: i64,
    #[serde(default)]
    pub total_bytes: i64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub unity_version: Option<String>,
    #[serde(default)]
    pub pubdate: Option<String>,
    #[serde(default)]
    pub store_id: Option<String>,
    #[serde(default)]
    pub category_label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub previews_indexed: bool,
    #[serde(default)]
    pub cached: bool,
}

impl Package {
    pub fn display_category(&self) -> &str {
        self.category_label.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.category)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Asset {
    pub id: i64,
    pub package_id: i64,
    pub guid: String,
    pub path: String,
    pub name: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub importer: Option<String>,
    #[serde(default)]
    pub main_class: Option<String>,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub is_folder: bool,
    #[serde(default)]
    pub has_preview: bool,
    #[serde(default)]
    pub is_text: bool,
    #[serde(default)]
    pub scan_truncated: bool,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub package_rel_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KindCount {
    pub kind: String,
    pub n: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Stats {
    pub packages: i64,
    pub assets: i64,
    pub refs: i64,
    pub bytes: i64,
    pub previews: i64,
    pub fts: bool,
    pub db: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchQuery {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub ext: Option<String>,
    #[serde(default)]
    pub include_folders: bool,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_limit() -> usize {
    50
}

/// One resolved or unresolved direct reference of an asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefInfo {
    pub guid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<Asset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetInfo {
    #[serde(flatten)]
    pub asset: Asset,
    pub refs: Vec<RefInfo>,
    pub referrers: Vec<Asset>,
    pub same_guid_in: Vec<Asset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageListing {
    pub package: Package,
    pub assets: Vec<Asset>,
    pub kinds: Vec<KindCount>,
}

/// A node in a dependency closure (an asset plus how it was reached).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosureAsset {
    #[serde(flatten)]
    pub asset: Asset,
    pub depth: u32,
    pub via: Option<String>,
    #[serde(default)]
    pub also_in_packages: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unresolved {
    pub guid: String,
    pub label: Option<String>,
    pub referrers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClosureOut {
    pub roots: Vec<String>,
    pub assets: Vec<ClosureAsset>,
    pub unresolved: Vec<Unresolved>,
    pub skipped_scripts: Vec<Asset>,
    pub total_bytes: i64,
    pub package_ids: Vec<i64>,
    pub packages: Vec<PackageRef>,
    /// guid -> child guids (resolved or not), for tree rendering.
    #[serde(default)]
    pub edges: std::collections::BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExportRequest {
    pub identifiers: Vec<String>,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default = "yes")]
    pub include_deps: bool,
    #[serde(default = "yes")]
    pub include_scripts: bool,
    #[serde(default = "yes")]
    pub include_folders: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootRef {
    pub guid: String,
    pub path: String,
    pub package: String,
}

/// What an export would do (the plan), independent of the destination.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanOut {
    pub roots: Vec<RootRef>,
    pub planned: usize,
    pub planned_bytes: i64,
    pub packages: Vec<String>,
    pub warnings: Vec<String>,
    pub unresolved: Vec<Unresolved>,
    pub scripts: Vec<String>,
    pub skipped_scripts: Vec<String>,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrittenFile {
    pub guid: String,
    pub path: String,
    pub bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub guid: String,
    pub path: String,
    pub existing_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingMember {
    pub guid: String,
    pub path: String,
    pub got: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExportResult {
    pub output: String,
    pub dry_run: bool,
    pub bytes_written: i64,
    pub seconds: f64,
    pub written: Vec<WrittenFile>,
    pub skipped_existing: Vec<WrittenFile>,
    pub conflicts: Vec<Conflict>,
    pub missing_in_package: Vec<MissingMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportOut {
    #[serde(flatten)]
    pub plan: PlanOut,
    pub result: ExportResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub package_id: i64,
    pub package: String,
    pub bytes: i64,
    pub complete: bool,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexReport {
    pub found: usize,
    pub indexed: usize,
    pub skipped: usize,
    pub removed: usize,
    pub errors: usize,
    pub entries: usize,
    pub previews: usize,
    pub seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextOut {
    pub guid: String,
    pub path: String,
    pub text: String,
    pub truncated: bool,
}

/// Error payload used by the HTTP API and mirrored by the remote client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<Asset>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub package_candidates: Vec<Package>,
}
