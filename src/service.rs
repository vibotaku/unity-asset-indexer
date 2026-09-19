//! The operations `uai` offers, behind one trait so the CLI (and the MCP server) can run them either
//! locally against the SQLite index + package files, or remotely against a `uai serve` instance.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Config;
use crate::db::Database;
use crate::deps::{known_guid_label, resolve_closure};
use crate::error::UaiError;
use crate::exporter::{self, build_plan, plan_out, Plan};
use crate::model::*;
use crate::resolve::{resolve_asset, resolve_package, resolve_roots};

/// Where an export goes.
#[derive(Debug, Clone)]
pub enum ExportDest {
    /// A Unity project (needs `Assets/` and `ProjectSettings/`); guid conflicts are checked.
    Project(PathBuf),
    /// A plain directory.
    Dir(PathBuf),
    /// A slim `.unitypackage` file.
    UnityPackage(PathBuf),
}

#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    pub force: bool,
    pub dry_run: bool,
    pub conflict_check: bool,
}

pub trait Service {
    fn is_remote(&self) -> bool;
    fn stats(&self) -> Result<Stats>;
    fn packages(&self) -> Result<Vec<Package>>;
    fn find_package(&self, needle: &str) -> Result<Package>;
    fn search(&self, q: &SearchQuery) -> Result<Vec<Asset>>;
    fn ls(
        &self,
        package: &str,
        prefix: Option<&str>,
        kind: Option<&str>,
        limit: Option<usize>,
    ) -> Result<PackageListing>;
    fn resolve(&self, ident: &str, package: Option<&str>) -> Result<Asset>;
    fn info(&self, ident: &str, package: Option<&str>) -> Result<AssetInfo>;
    fn deps(
        &self,
        idents: &[String],
        package: Option<&str>,
        include_scripts: bool,
        max_depth: Option<u32>,
    ) -> Result<ClosureOut>;
    fn rdeps(&self, ident: &str, package: Option<&str>, limit: Option<usize>) -> Result<Vec<Asset>>;
    fn text(&self, ident: &str, package: Option<&str>, max_bytes: usize) -> Result<TextOut>;
    fn preview(&self, ident: &str, package: Option<&str>) -> Result<Vec<u8>>;
    fn plan(&self, req: &ExportRequest) -> Result<PlanOut>;
    fn export(
        &self,
        req: &ExportRequest,
        dest: &ExportDest,
        opts: &ExportOptions,
        log: &mut dyn FnMut(&str),
    ) -> Result<ExportOut>;
}

pub fn validate_project(dir: &Path) -> Result<PathBuf> {
    let proj = if dir.is_absolute() { dir.to_path_buf() } else { std::env::current_dir()?.join(dir) };
    if !proj.join("Assets").is_dir() || !proj.join("ProjectSettings").is_dir() {
        return Err(UaiError::Invalid(format!(
            "{} does not look like a Unity project (needs Assets/ and ProjectSettings/)",
            proj.display()
        ))
        .into());
    }
    Ok(proj)
}

// ----- local ---------------------------------------------------------------------------------------

pub struct LocalService {
    pub cfg: Config,
    pub db: Database,
}

impl LocalService {
    pub fn open(cfg: &Config) -> Result<LocalService> {
        let db = Database::open_with_previews(&cfg.db_path(), &cfg.previews_path())?;
        Ok(LocalService { cfg: cfg.clone(), db })
    }

    pub fn build_plan(&self, req: &ExportRequest) -> Result<(Vec<Asset>, Plan)> {
        if req.identifiers.is_empty() {
            return Err(UaiError::Invalid("no assets given".into()).into());
        }
        let roots = resolve_roots(&self.db, &req.identifiers, req.package.as_deref())?;
        let plan = build_plan(&self.db, &roots, req.include_deps, req.include_scripts, req.include_folders)?;
        Ok((roots, plan))
    }

    pub fn asset_info(&self, asset: Asset) -> Result<AssetInfo> {
        let mut refs = Vec::new();
        for g in self.db.refs_of(asset.id)? {
            let cands = self.db.assets_by_guid(&g)?;
            let preferred = cands.iter().find(|c| c.package_id == asset.package_id).or(cands.first()).cloned();
            refs.push(RefInfo {
                guid: g.clone(),
                label: if preferred.is_none() { known_guid_label(&g) } else { None },
                asset: preferred,
            });
        }
        let referrers = self.db.referrers(&asset.guid, Some(200))?;
        let same_guid_in: Vec<Asset> =
            self.db.assets_by_guid(&asset.guid)?.into_iter().filter(|a| a.id != asset.id).collect();
        Ok(AssetInfo { asset, refs, referrers, same_guid_in })
    }

    /// Preview bytes from the thumbnail store. When `extract` is set (or the package is cached
    /// locally, which makes it cheap), fall back to pulling `preview.png` out of the package and
    /// store it for next time.
    pub fn preview_for(&mut self, asset: &Asset, extract: bool) -> Result<Option<Vec<u8>>> {
        if let Some(png) = self.db.get_preview(&asset.guid)? {
            return Ok(Some(png));
        }
        if !asset.has_preview || !(extract || exporter::cache_complete(&self.cfg, asset.package_id)) {
            return Ok(None);
        }
        let png = exporter::extract_preview(&self.cfg, &self.db, asset)?;
        if let Some(p) = &png {
            let _ = self.db.put_previews(&[(asset.guid.clone(), p.clone())]);
        }
        Ok(png)
    }

    pub fn text_for(&self, asset: &Asset, max_bytes: usize) -> Result<TextOut> {
        if !asset.is_text {
            return Err(UaiError::Invalid(format!(
                "{} is binary ({} bytes); use `uai export`",
                asset.path, asset.size
            ))
            .into());
        }
        let (text, truncated) = exporter::extract_text(&self.cfg, &self.db, asset, max_bytes)?
            .ok_or_else(|| UaiError::NotFound("asset not found in package (index stale? run `uai index`)".into()))?;
        Ok(TextOut { guid: asset.guid.clone(), path: asset.path.clone(), text, truncated })
    }

    /// Raw asset bytes (for serving textures / audio in the web UI).
    pub fn raw_for(&self, asset: &Asset, max_bytes: usize) -> Result<Option<(Vec<u8>, bool)>> {
        exporter::read_member(&self.cfg, &self.db, asset, "asset", max_bytes)
    }

    pub fn cache_list(&self) -> Result<Vec<CacheEntry>> {
        exporter::cache_list(&self.cfg, &self.db)
    }
}

impl Service for LocalService {
    fn is_remote(&self) -> bool {
        false
    }

    fn stats(&self) -> Result<Stats> {
        self.db.stats()
    }

    fn packages(&self) -> Result<Vec<Package>> {
        let mut pkgs = self.db.packages()?;
        for p in &mut pkgs {
            p.cached = exporter::cache_complete(&self.cfg, p.id);
        }
        Ok(pkgs)
    }

    fn find_package(&self, needle: &str) -> Result<Package> {
        let mut p = resolve_package(&self.db, needle)?;
        p.cached = exporter::cache_complete(&self.cfg, p.id);
        Ok(p)
    }

    fn search(&self, q: &SearchQuery) -> Result<Vec<Asset>> {
        self.db.search(q)
    }

    fn ls(
        &self,
        package: &str,
        prefix: Option<&str>,
        kind: Option<&str>,
        limit: Option<usize>,
    ) -> Result<PackageListing> {
        let pkg = self.find_package(package)?;
        let assets = self.db.list_package_assets(pkg.id, prefix, kind, limit)?;
        let kinds = self.db.kind_counts(Some(pkg.id))?;
        Ok(PackageListing { package: pkg, assets, kinds })
    }

    fn resolve(&self, ident: &str, package: Option<&str>) -> Result<Asset> {
        resolve_asset(&self.db, ident, package)
    }

    fn info(&self, ident: &str, package: Option<&str>) -> Result<AssetInfo> {
        let asset = resolve_asset(&self.db, ident, package)?;
        self.asset_info(asset)
    }

    fn deps(
        &self,
        idents: &[String],
        package: Option<&str>,
        include_scripts: bool,
        max_depth: Option<u32>,
    ) -> Result<ClosureOut> {
        let roots = resolve_roots(&self.db, idents, package)?;
        let cl = resolve_closure(&self.db, &roots, include_scripts, max_depth)?;
        cl.to_out(&self.db)
    }

    fn rdeps(&self, ident: &str, package: Option<&str>, limit: Option<usize>) -> Result<Vec<Asset>> {
        let asset = resolve_asset(&self.db, ident, package)?;
        self.db.referrers(&asset.guid, limit)
    }

    fn text(&self, ident: &str, package: Option<&str>, max_bytes: usize) -> Result<TextOut> {
        let asset = resolve_asset(&self.db, ident, package)?;
        self.text_for(&asset, max_bytes)
    }

    fn preview(&self, ident: &str, package: Option<&str>) -> Result<Vec<u8>> {
        let asset = resolve_asset(&self.db, ident, package)?;
        if let Some(png) = self.db.get_preview(&asset.guid)? {
            return Ok(png);
        }
        if !asset.has_preview {
            return Err(UaiError::NotFound(format!("{} has no preview.png in its package", asset.path)).into());
        }
        exporter::extract_preview(&self.cfg, &self.db, &asset)?
            .ok_or_else(|| UaiError::NotFound("preview not found in package".into()).into())
    }

    fn plan(&self, req: &ExportRequest) -> Result<PlanOut> {
        let (roots, plan) = self.build_plan(req)?;
        plan_out(&self.db, &plan, &roots)
    }

    fn export(
        &self,
        req: &ExportRequest,
        dest: &ExportDest,
        opts: &ExportOptions,
        log: &mut dyn FnMut(&str),
    ) -> Result<ExportOut> {
        let (roots, plan) = self.build_plan(req)?;
        let result = match dest {
            ExportDest::Project(dir) => {
                let proj = validate_project(dir)?;
                let guids = if opts.conflict_check { exporter::scan_project_guids(&proj) } else { HashMap::new() };
                exporter::export_to_dir(&self.cfg, &self.db, &plan, &proj, opts.force, opts.dry_run, &guids, log)?
            }
            ExportDest::Dir(dir) => exporter::export_to_dir(
                &self.cfg,
                &self.db,
                &plan,
                dir,
                opts.force,
                opts.dry_run,
                &HashMap::new(),
                log,
            )?,
            ExportDest::UnityPackage(file) => {
                exporter::export_to_unitypackage(&self.cfg, &self.db, &plan, file, opts.dry_run, log)?
            }
        };
        Ok(ExportOut { plan: plan_out(&self.db, &plan, &roots)?, result })
    }
}

/// Open the right service for the configuration: remote when a server is configured, else local.
pub fn open_service(cfg: &Config) -> Result<Box<dyn Service>> {
    if let Some(url) = &cfg.server {
        return Ok(Box::new(crate::client::RemoteService::new(url)?));
    }
    Ok(Box::new(LocalService::open(cfg).context("opening the index")?))
}
