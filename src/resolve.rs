//! Identifier resolution shared by the CLI, the HTTP API and the MCP server.
//!
//! An asset identifier is a 32-hex guid, `Package::Assets/full/path`, an `Assets/...` path, a path
//! suffix, a bare file name, or `#<asset id>`. A package identifier is an id or a (partial) name.

use anyhow::Result;

use crate::db::Database;
use crate::error::UaiError;
use crate::model::{Asset, Package};
use crate::unitypackage::is_guid;

pub fn resolve_package(db: &Database, needle: &str) -> Result<Package> {
    let rows = db.find_packages(needle)?;
    match rows.len() {
        0 => Err(UaiError::NotFound(format!("no package matches {needle:?}")).into()),
        1 => Ok(rows.into_iter().next().unwrap()),
        _ => Err(UaiError::ambiguous_packages(
            format!("{needle:?} matches several packages; use the id or a longer name:"),
            rows,
        )
        .into()),
    }
}

pub fn resolve_asset(db: &Database, ident: &str, package: Option<&str>) -> Result<Asset> {
    let mut ident = ident.trim().to_string();
    let mut pkg: Option<Package> = None;
    if let Some((pkg_name, rest)) = ident.split_once("::") {
        pkg = Some(resolve_package(db, pkg_name)?);
        ident = rest.to_string();
    } else if let Some(p) = package.filter(|p| !p.trim().is_empty()) {
        pkg = Some(resolve_package(db, p)?);
    }
    let pid = pkg.as_ref().map(|p| p.id);
    let rows: Vec<Asset> = if let Some(id) = ident.strip_prefix('#').and_then(|s| s.parse::<i64>().ok()) {
        db.asset_by_id(id)?.into_iter().filter(|a| pid.map(|p| p == a.package_id).unwrap_or(true)).collect()
    } else if is_guid(&ident) {
        db.assets_by_guid(&ident)?.into_iter().filter(|a| pid.map(|p| p == a.package_id).unwrap_or(true)).collect()
    } else {
        let norm = ident.replace('\\', "/").trim_matches('/').to_string();
        let mut rows = db.assets_by_path(&norm, pid)?;
        if rows.is_empty() {
            rows = db.assets_by_path_suffix(&norm, pid, 50)?;
        }
        if rows.is_empty() {
            rows = db.assets_by_name(&norm, pid, 50)?;
        }
        rows
    };
    if rows.is_empty() {
        let hint = ident.rsplit('/').next().unwrap_or(&ident).to_string();
        return Err(UaiError::NotFound(format!("no asset matches {ident:?}. Try `uai search {hint:?}`.")).into());
    }
    let mut distinct: Vec<(i64, &str)> = rows.iter().map(|r| (r.package_id, r.guid.as_str())).collect();
    distinct.sort();
    distinct.dedup();
    if distinct.len() > 1 {
        let n = rows.len();
        return Err(UaiError::ambiguous_assets(
            format!("{ident:?} is ambiguous ({n} matches). Use the guid or `Package::Assets/path`:"),
            rows,
        )
        .into());
    }
    Ok(rows.into_iter().next().unwrap())
}

/// Resolve several identifiers, dropping duplicates (same package + guid).
pub fn resolve_roots(db: &Database, idents: &[String], package: Option<&str>) -> Result<Vec<Asset>> {
    let mut roots: Vec<Asset> = Vec::new();
    for i in idents {
        let a = resolve_asset(db, i, package)?;
        if !roots.iter().any(|r| r.package_id == a.package_id && r.guid == a.guid) {
            roots.push(a);
        }
    }
    Ok(roots)
}
