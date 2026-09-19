//! Export assets (plus dependencies) into a Unity project, a plain folder, or a slim `.unitypackage`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use walkdir::WalkDir;

use crate::config::Config;
use crate::db::Database;
use crate::deps::{resolve_closure, Closure};
use crate::model::{Asset, CacheEntry, Conflict, ExportResult, MissingMember, Package, PlanOut, RootRef, WrittenFile};
use crate::unitypackage::{for_each_member, open_stream, split_member_name, Member};

#[derive(Debug, Clone)]
pub struct Plan {
    pub closure: Closure,
    /// everything to write (roots + deps + folder metas)
    pub assets: Vec<Asset>,
    /// subset: folder assets (meta only)
    pub folders: Vec<Asset>,
    pub by_package: BTreeMap<i64, Vec<Asset>>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn total_bytes(&self) -> i64 {
        self.assets.iter().map(|a| a.size).sum()
    }
    pub fn all_guids(&self) -> HashSet<String> {
        self.assets.iter().map(|a| a.guid.clone()).collect()
    }
}

pub fn build_plan(
    db: &Database,
    roots: &[Asset],
    include_deps: bool,
    include_scripts: bool,
    include_folders: bool,
) -> Result<Plan> {
    let closure = resolve_closure(db, roots, include_scripts, if include_deps { None } else { Some(0) })?;
    let assets: Vec<Asset> = closure.assets().cloned().collect();
    let mut folders: Vec<Asset> = Vec::new();
    if include_folders {
        let mut by_pkg_dirs: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
        for a in &assets {
            let parts: Vec<&str> = a.path.split('/').filter(|p| !p.is_empty()).collect();
            for i in 1..parts.len() {
                by_pkg_dirs.entry(a.package_id).or_default().insert(parts[..i].join("/"));
            }
        }
        let mut have: HashSet<(i64, String)> = assets.iter().map(|a| (a.package_id, a.guid.clone())).collect();
        for (pid, dirs) in by_pkg_dirs {
            let dirs: Vec<String> = dirs.into_iter().collect();
            for f in db.folder_assets_for_paths(pid, &dirs)? {
                if have.insert((pid, f.guid.clone())) {
                    folders.push(f);
                }
            }
        }
    }
    let mut all = assets;
    all.extend(folders.iter().cloned());
    let mut by_package: BTreeMap<i64, Vec<Asset>> = BTreeMap::new();
    for a in &all {
        by_package.entry(a.package_id).or_default().push(a.clone());
    }
    let mut warnings = Vec::new();
    let scripts = closure.scripts();
    if !scripts.is_empty() {
        warnings.push(format!(
            "{} script/plugin file(s) are in the dependency set. Partial script imports may not compile if they depend on \
             other scripts in the package (`--no-scripts` to leave them out).",
            scripts.len()
        ));
    }
    if !closure.unresolved.is_empty() {
        warnings
            .push(format!("{} referenced guid(s) are not in the library (see unresolved).", closure.unresolved.len()));
    }
    let amb = closure.nodes.iter().filter(|n| !n.ambiguous_in.is_empty()).count();
    if amb > 0 {
        warnings.push(format!("{amb} guid(s) exist in more than one package; the referrer's package was preferred."));
    }
    Ok(Plan { closure, assets: all, folders, by_package, warnings })
}

pub fn plan_out(db: &Database, plan: &Plan, roots: &[Asset]) -> Result<PlanOut> {
    let mut packages = Vec::new();
    for pid in plan.by_package.keys() {
        if let Some(p) = db.package_by_id(*pid)? {
            packages.push(p.name);
        }
    }
    Ok(PlanOut {
        roots: roots
            .iter()
            .map(|r| RootRef { guid: r.guid.clone(), path: r.path.clone(), package: r.package.clone() })
            .collect(),
        planned: plan.assets.len(),
        planned_bytes: plan.total_bytes(),
        packages,
        warnings: plan.warnings.clone(),
        unresolved: plan.closure.unresolved.values().cloned().collect(),
        scripts: plan.closure.scripts().iter().map(|n| n.asset.path.clone()).collect(),
        skipped_scripts: plan.closure.skipped_scripts.values().map(|a| a.path.clone()).collect(),
        assets: plan.assets.clone(),
    })
}

// ----- reading members from a package (cache or stream) -------------------------------------------

pub fn package_abs_path(cfg: &Config, pkg: &Package) -> PathBuf {
    cfg.library.join(&pkg.rel_path)
}

pub fn cache_path_for(cfg: &Config, pkg_id: i64) -> PathBuf {
    cfg.cache_dir().join(pkg_id.to_string())
}

pub fn cache_complete(cfg: &Config, pkg_id: i64) -> bool {
    cache_path_for(cfg, pkg_id).join(".complete").is_file()
}

const LEAVES: &[&str] = &["asset", "asset.meta", "pathname", "preview.png"];

/// Call `f` for every member of the wanted guids. Reads from the local cache when present, else streams
/// the package from the library.
pub fn for_each_package_member<F>(cfg: &Config, pkg: &Package, guids: &HashSet<String>, mut f: F) -> Result<()>
where
    F: FnMut(Member<'_>) -> Result<()>,
{
    let cache = cache_path_for(cfg, pkg.id);
    if cache.join(".complete").is_file() {
        let mut sorted: Vec<&String> = guids.iter().collect();
        sorted.sort();
        for g in sorted {
            let d = cache.join(g.to_ascii_lowercase());
            if !d.is_dir() {
                continue;
            }
            for leaf in LEAVES {
                let p = d.join(leaf);
                let Ok(md) = fs::metadata(&p) else { continue };
                if !md.is_file() {
                    continue;
                }
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let mut fh = File::open(&p)?;
                f(Member {
                    guid: g.to_ascii_lowercase(),
                    leaf: leaf.to_string(),
                    size: md.len(),
                    mtime,
                    reader: &mut fh,
                })?;
            }
        }
        return Ok(());
    }
    let path = package_abs_path(cfg, pkg);
    if !path.is_file() {
        anyhow::bail!(
            "package file not found: {} (library not mounted? try `uai config` / `uai cache add`)",
            path.display()
        );
    }
    for_each_member(&path, guids, f)
}

// ----- cache ---------------------------------------------------------------------------------------

pub fn cache_add(cfg: &Config, pkg: &Package, log: &mut dyn FnMut(&str)) -> Result<PathBuf> {
    let dest = cache_path_for(cfg, pkg.id);
    if dest.join(".complete").is_file() {
        return Ok(dest);
    }
    let tmp = cfg.cache_dir().join(format!("{}.partial", pkg.id));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;
    let src = package_abs_path(cfg, pkg);
    let size = fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
    log(&format!("caching {} ({:.0} MB compressed) -> {}", pkg.name, size as f64 / 1e6, dest.display()));
    let t0 = Instant::now();
    let mut archive = open_stream(&src, None)?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = match entry.path() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => continue,
        };
        let Some((guid, Some(leaf))) = split_member_name(&name) else { continue };
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let dir = tmp.join(&guid);
        fs::create_dir_all(&dir)?;
        let out_path = dir.join(&leaf);
        let mut out = BufWriter::new(File::create(&out_path)?);
        io::copy(&mut entry, &mut out)?;
        out.flush()?;
    }
    fs::write(tmp.join(".complete"), crate::db::now().to_string())?;
    if dest.exists() {
        fs::remove_dir_all(&dest)?;
    }
    fs::rename(&tmp, &dest)?;
    log(&format!("cached in {:.0}s", t0.elapsed().as_secs_f64()));
    Ok(dest)
}

pub fn cache_remove(cfg: &Config, pkg_id: i64) -> Result<bool> {
    let dest = cache_path_for(cfg, pkg_id);
    if dest.exists() {
        fs::remove_dir_all(&dest)?;
        return Ok(true);
    }
    Ok(false)
}

pub fn cache_list(cfg: &Config, db: &Database) -> Result<Vec<CacheEntry>> {
    let mut out = Vec::new();
    let dir = cfg.cache_dir();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut entries: Vec<_> = fs::read_dir(&dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let Ok(pid) = name.parse::<i64>() else { continue };
        if !e.path().is_dir() {
            continue;
        }
        let pkg = db.package_by_id(pid)?;
        let bytes: u64 = WalkDir::new(e.path())
            .into_iter()
            .filter_map(|x| x.ok())
            .filter(|x| x.file_type().is_file())
            .filter_map(|x| x.metadata().ok())
            .map(|m| m.len())
            .sum();
        out.push(CacheEntry {
            package_id: pid,
            package: pkg.map(|p| p.name).unwrap_or_else(|| "?".into()),
            bytes: bytes as i64,
            complete: e.path().join(".complete").is_file(),
            path: e.path().to_string_lossy().to_string(),
        });
    }
    Ok(out)
}

// ----- project conflict scan -------------------------------------------------------------------------

/// guid -> relative path (without .meta) for every .meta under Assets/.
pub fn scan_project_guids(project_dir: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let assets = project_dir.join("Assets");
    if !assets.is_dir() {
        return out;
    }
    for e in WalkDir::new(&assets)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let name = e.file_name().to_string_lossy();
        if !name.ends_with(".meta") {
            continue;
        }
        let Ok(mut f) = File::open(e.path()) else { continue };
        let mut head = [0u8; 512];
        let mut n = 0;
        while n < head.len() {
            match f.read(&mut head[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&head[..n]);
        if let Some(g) = meta_guid(&text) {
            let rel = e.path().strip_prefix(project_dir).unwrap_or(e.path()).to_string_lossy().replace('\\', "/");
            out.insert(g, rel[..rel.len() - ".meta".len()].to_string());
        }
    }
    out
}

fn meta_guid(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("guid:") {
            let g = rest.trim();
            if crate::unitypackage::is_guid(g) {
                return Some(g.to_ascii_lowercase());
            }
        }
    }
    None
}

// ----- writing -------------------------------------------------------------------------------------

fn set_mtime(path: &Path, mtime: u64) {
    if mtime > 0 {
        let ft = filetime::FileTime::from_unix_time(mtime as i64, 0);
        let _ = filetime::set_file_mtime(path, ft);
    }
}

fn meta_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".meta");
    PathBuf::from(s)
}

/// Decide which of `assets` need writing (skips existing files unless `force`, records conflicts).
fn select_todo(
    assets: &[Asset],
    dest_root: &Path,
    force: bool,
    project_guids: &HashMap<String, String>,
    res: &mut ExportResult,
) -> HashMap<String, Asset> {
    let mut todo = HashMap::new();
    for a in assets {
        let target = dest_root.join(&a.path);
        if let Some(existing) = project_guids.get(&a.guid) {
            if !existing.eq_ignore_ascii_case(&a.path) {
                res.conflicts.push(Conflict {
                    guid: a.guid.clone(),
                    path: a.path.clone(),
                    existing_path: existing.clone(),
                });
                continue;
            }
        }
        let exists = if a.is_folder { meta_path(&target).exists() } else { target.exists() };
        if exists && !force {
            res.skipped_existing.push(WrittenFile { guid: a.guid.clone(), path: a.path.clone(), bytes: a.size });
            continue;
        }
        todo.insert(a.guid.clone(), a.clone());
    }
    todo
}

/// Write members of `todo` coming from `source` under `dest_root`.
fn write_members<S>(todo: &HashMap<String, Asset>, dest_root: &Path, res: &mut ExportResult, source: S) -> Result<()>
where
    S: FnOnce(&HashSet<String>, &mut dyn FnMut(Member<'_>) -> Result<()>) -> Result<()>,
{
    let wanted: HashSet<String> = todo.keys().cloned().collect();
    let mut seen: HashMap<String, BTreeSet<String>> = todo.keys().map(|g| (g.clone(), BTreeSet::new())).collect();
    let mut bytes = 0i64;
    source(&wanted, &mut |m: Member<'_>| {
        let Some(a) = todo.get(&m.guid) else { return Ok(()) };
        let target = dest_root.join(&a.path);
        match m.leaf.as_str() {
            "asset" => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut out =
                    BufWriter::new(File::create(&target).with_context(|| format!("creating {}", target.display()))?);
                io::copy(m.reader, &mut out)?;
                out.flush()?;
                drop(out);
                set_mtime(&target, m.mtime);
                bytes += m.size as i64;
                seen.get_mut(&m.guid).unwrap().insert("asset".into());
            }
            "asset.meta" => {
                let mp = meta_path(&target);
                if let Some(parent) = mp.parent() {
                    fs::create_dir_all(parent)?;
                }
                if a.is_folder {
                    fs::create_dir_all(&target)?;
                }
                let mut out = File::create(&mp).with_context(|| format!("creating {}", mp.display()))?;
                io::copy(m.reader, &mut out)?;
                seen.get_mut(&m.guid).unwrap().insert("meta".into());
            }
            _ => {}
        }
        Ok(())
    })?;
    res.bytes_written += bytes;
    let mut guids: Vec<&String> = todo.keys().collect();
    guids.sort();
    for g in guids {
        let a = &todo[g];
        let got = &seen[g];
        if got.contains("meta") && (got.contains("asset") || a.is_folder) {
            res.written.push(WrittenFile { guid: g.clone(), path: a.path.clone(), bytes: a.size });
        } else {
            res.missing_in_package.push(MissingMember {
                guid: g.clone(),
                path: a.path.clone(),
                got: got.iter().cloned().collect(),
            });
        }
    }
    Ok(())
}

/// Write `<dest_root>/<Assets/...>` and `.meta` for every planned asset, streaming from the library.
#[allow(clippy::too_many_arguments)]
pub fn export_to_dir(
    cfg: &Config,
    db: &Database,
    plan: &Plan,
    dest_root: &Path,
    force: bool,
    dry_run: bool,
    project_guids: &HashMap<String, String>,
    log: &mut dyn FnMut(&str),
) -> Result<ExportResult> {
    let t0 = Instant::now();
    let mut res = ExportResult { dry_run, output: dest_root.to_string_lossy().to_string(), ..Default::default() };
    for (pid, assets) in &plan.by_package {
        let pkg = db.package_by_id(*pid)?.with_context(|| format!("package {pid} vanished from the index"))?;
        let todo = select_todo(assets, dest_root, force, project_guids, &mut res);
        if todo.is_empty() {
            continue;
        }
        log(&format!("{} {} from {}", if dry_run { "would extract" } else { "extracting" }, todo.len(), pkg.name));
        if dry_run {
            let mut items: Vec<&Asset> = todo.values().collect();
            items.sort_by(|a, b| a.path.cmp(&b.path));
            for a in items {
                res.written.push(WrittenFile { guid: a.guid.clone(), path: a.path.clone(), bytes: a.size });
                res.bytes_written += a.size;
            }
            continue;
        }
        write_members(&todo, dest_root, &mut res, |wanted, f| for_each_package_member(cfg, &pkg, wanted, f))?;
    }
    res.seconds = t0.elapsed().as_secs_f64();
    Ok(res)
}

/// Unpack a (slim) `.unitypackage` file into `dest_root`, writing only the planned `assets`.
/// Used by the remote client after downloading a package built by the server.
pub fn unpack_package_to_dir(
    package_file: &Path,
    assets: &[Asset],
    dest_root: &Path,
    force: bool,
    project_guids: &HashMap<String, String>,
) -> Result<ExportResult> {
    let t0 = Instant::now();
    let mut res = ExportResult { output: dest_root.to_string_lossy().to_string(), ..Default::default() };
    let todo = select_todo(assets, dest_root, force, project_guids, &mut res);
    if !todo.is_empty() {
        write_members(&todo, dest_root, &mut res, |wanted, f| for_each_member(package_file, wanted, f))?;
    }
    res.seconds = t0.elapsed().as_secs_f64();
    Ok(res)
}

/// Repack the selected guid directories into a new (much smaller) `.unitypackage`, written to `w`.
pub fn write_unitypackage<W: Write>(
    cfg: &Config,
    db: &Database,
    plan: &Plan,
    w: W,
    log: &mut dyn FnMut(&str),
) -> Result<ExportResult> {
    let t0 = Instant::now();
    let mut res = ExportResult::default();
    let gz = GzEncoder::new(w, Compression::new(6));
    let mut tar = tar::Builder::new(gz);
    tar.mode(tar::HeaderMode::Deterministic);
    for (pid, assets) in &plan.by_package {
        let pkg = db.package_by_id(*pid)?.with_context(|| format!("package {pid} vanished from the index"))?;
        let todo: HashMap<String, Asset> = assets.iter().map(|a| (a.guid.clone(), a.clone())).collect();
        let wanted: HashSet<String> = todo.keys().cloned().collect();
        log(&format!("packing {} from {}", todo.len(), pkg.name));
        let mut seen: HashMap<String, BTreeSet<String>> = todo.keys().map(|g| (g.clone(), BTreeSet::new())).collect();
        for_each_package_member(cfg, &pkg, &wanted, |m| {
            let mut h = tar::Header::new_gnu();
            h.set_size(m.size);
            h.set_mtime(m.mtime);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            tar.append_data(&mut h, format!("{}/{}", m.guid, m.leaf), m.reader)?;
            if m.leaf == "asset" {
                res.bytes_written += m.size as i64;
            }
            seen.get_mut(&m.guid).unwrap().insert(m.leaf.clone());
            Ok(())
        })?;
        let mut guids: Vec<&String> = todo.keys().collect();
        guids.sort();
        for g in guids {
            let a = &todo[g];
            if seen[g].contains("asset.meta") {
                res.written.push(WrittenFile { guid: g.clone(), path: a.path.clone(), bytes: a.size });
            } else {
                res.missing_in_package.push(MissingMember {
                    guid: g.clone(),
                    path: a.path.clone(),
                    got: seen[g].iter().cloned().collect(),
                });
            }
        }
    }
    let gz = tar.into_inner()?;
    let mut w = gz.finish()?;
    w.flush()?;
    res.seconds = t0.elapsed().as_secs_f64();
    Ok(res)
}

pub fn export_to_unitypackage(
    cfg: &Config,
    db: &Database,
    plan: &Plan,
    out_file: &Path,
    dry_run: bool,
    log: &mut dyn FnMut(&str),
) -> Result<ExportResult> {
    let t0 = Instant::now();
    if dry_run {
        let mut res =
            ExportResult { dry_run: true, output: out_file.to_string_lossy().to_string(), ..Default::default() };
        for a in &plan.assets {
            res.written.push(WrittenFile { guid: a.guid.clone(), path: a.path.clone(), bytes: a.size });
            res.bytes_written += a.size;
        }
        res.seconds = t0.elapsed().as_secs_f64();
        return Ok(res);
    }
    if let Some(parent) = out_file.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = PathBuf::from(format!("{}.partial", out_file.to_string_lossy()));
    let file = BufWriter::new(File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?);
    let mut res = write_unitypackage(cfg, db, plan, file, log)?;
    fs::rename(&tmp, out_file)?;
    res.output = out_file.to_string_lossy().to_string();
    res.seconds = t0.elapsed().as_secs_f64();
    Ok(res)
}

/// Read one member (`asset`, `preview.png`, ...) of an asset into memory, up to `max_bytes`.
/// Returns (bytes, truncated).
pub fn read_member(
    cfg: &Config,
    db: &Database,
    asset: &Asset,
    leaf: &str,
    max_bytes: usize,
) -> Result<Option<(Vec<u8>, bool)>> {
    let pkg = db.package_by_id(asset.package_id)?.context("package vanished from the index")?;
    let mut out: Option<(Vec<u8>, bool)> = None;
    let wanted: HashSet<String> = [asset.guid.clone()].into_iter().collect();
    for_each_package_member(cfg, &pkg, &wanted, |m| {
        if m.leaf == leaf && out.is_none() {
            let mut buf = Vec::with_capacity((m.size as usize).min(max_bytes));
            let n = m.reader.take(max_bytes as u64).read_to_end(&mut buf)?;
            out = Some((buf, (n as u64) < m.size));
        }
        Ok(())
    })?;
    Ok(out)
}

pub fn extract_preview(cfg: &Config, db: &Database, asset: &Asset) -> Result<Option<Vec<u8>>> {
    Ok(read_member(cfg, db, asset, "preview.png", 64 * 1024 * 1024)?.map(|(b, _)| b))
}

/// Return the (text) content of an asset, e.g. to read a prefab's YAML or a script.
pub fn extract_text(cfg: &Config, db: &Database, asset: &Asset, max_bytes: usize) -> Result<Option<(String, bool)>> {
    Ok(read_member(cfg, db, asset, "asset", max_bytes)?.map(|(b, t)| (String::from_utf8_lossy(&b).into_owned(), t)))
}
