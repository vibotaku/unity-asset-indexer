//! Walk the library, stream each `.unitypackage` once (in parallel), and store results in SQLite.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use walkdir::WalkDir;

use crate::config::Config;
use crate::db::Database;
use crate::model::IndexReport;
use crate::unitypackage::{read_package_header, scan_package, Entry, ScanOptions};

/// Previews are shipped from the workers to the writer in chunks of roughly this many bytes.
const PREVIEW_CHUNK_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub workers: usize,
    pub force: bool,
    /// Only packages whose relative path contains this (case-insensitive).
    pub only: Option<String>,
    /// Store `preview.png` thumbnails (needed by the web UI / `uai preview` without the share).
    pub previews: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        IndexOptions { workers: cpus.clamp(1, 6), force: false, only: None, previews: true }
    }
}

pub fn fmt_bytes(n: f64) -> String {
    let mut n = n;
    for unit in ["B", "KB", "MB", "GB"] {
        if n < 1024.0 || unit == "GB" {
            return if unit == "B" || unit == "KB" { format!("{n:.0}{unit}") } else { format!("{n:.2}{unit}") };
        }
        n /= 1024.0;
    }
    format!("{n:.2}GB")
}

pub fn fmt_eta(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

pub fn discover_packages(library: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = WalkDir::new(library)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.file_name().to_string_lossy().to_ascii_lowercase().ends_with(".unitypackage"))
        .map(|e| e.into_path())
        .collect();
    out.sort();
    out
}

/// `Publisher/Category/Name.unitypackage` -> (name, publisher, category). Tolerates flatter layouts.
pub fn split_rel_path(rel: &str) -> (String, String, String) {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut name = parts.last().copied().unwrap_or("").to_string();
    if name.to_ascii_lowercase().ends_with(".unitypackage") {
        name.truncate(name.len() - ".unitypackage".len());
    }
    let publisher = if parts.len() >= 2 { parts[0].to_string() } else { String::new() };
    let category = if parts.len() >= 3 { parts[1..parts.len() - 1].join("/") } else { String::new() };
    (name, publisher, category)
}

pub fn rel_path(library: &Path, p: &Path) -> String {
    p.strip_prefix(library).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

enum Msg {
    Previews(Vec<(String, Vec<u8>)>),
    Done { pid: i64, rel: String, entries: Vec<Entry>, seconds: f64, error: Option<String> },
}

fn mtime_secs(md: &std::fs::Metadata) -> f64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Index the library. `log` gets one line per finished package; `progress` (optional) gets a
/// periodically refreshed status line (bytes read, throughput, ETA).
pub fn index_library(
    cfg: &Config,
    db: &mut Database,
    opts: &IndexOptions,
    log: &mut dyn FnMut(&str),
    mut progress: Option<&mut dyn FnMut(&str)>,
) -> Result<IndexReport> {
    let lib = &cfg.library;
    if !cfg.library_mounted() {
        bail!(
            "Library not found: {} (is the share mounted? set UAI_LIBRARY or `uai config --library <dir>`)",
            lib.display()
        );
    }
    let mut paths = discover_packages(lib);
    if let Some(only) = opts.only.as_deref().map(|s| s.to_ascii_lowercase()) {
        paths.retain(|p| rel_path(lib, p).to_ascii_lowercase().contains(&only));
    }
    let mut seen_rel: Vec<String> = Vec::new();
    let mut todo: Vec<(i64, PathBuf, u64)> = Vec::new();
    let mut skipped = 0usize;
    for p in &paths {
        let rel = rel_path(lib, p);
        seen_rel.push(rel.clone());
        let md = match std::fs::metadata(p) {
            Ok(m) => m,
            Err(e) => {
                log(&format!("- cannot stat {rel}: {e}"));
                continue;
            }
        };
        let size = md.len() as i64;
        let mtime = mtime_secs(&md);
        let existing = db.package_by_rel_path(&rel)?;
        let (name, publisher, category) = split_rel_path(&rel);
        let changed = match &existing {
            None => true,
            Some(ex) => ex.size != size || (ex.mtime - mtime).abs() >= 1.0,
        };
        let need_header = opts.force || changed || existing.as_ref().map(|e| e.title.is_none()).unwrap_or(true);
        let header = if need_header { read_package_header(p) } else { None };
        let pid = db.upsert_package(&rel, &name, &publisher, &category, size, mtime, header.as_ref())?;
        let up_to_date = match &existing {
            Some(ex) => {
                !opts.force
                    && ex.status == "ok"
                    && ex.indexed_at.is_some()
                    && !changed
                    && (!opts.previews || ex.previews_indexed)
            }
            None => false,
        };
        if up_to_date {
            skipped += 1;
            continue;
        }
        todo.push((pid, p.clone(), md.len()));
    }

    let mut removed = 0usize;
    if opts.only.is_none() {
        for row in db.packages()? {
            if !seen_rel.contains(&row.rel_path) {
                log(&format!("- removed from library: {}", row.rel_path));
                db.delete_package(row.id)?;
                removed += 1;
            }
        }
    }

    log(&format!("{} packages found, {skipped} up to date, {} to index, {removed} removed", paths.len(), todo.len()));
    // Largest first so the long tail doesn't end up serialized on one worker at the end.
    todo.sort_by_key(|t| std::cmp::Reverse(t.2));
    let total_bytes: u64 = todo.iter().map(|t| t.2).sum();
    let n_todo = todo.len();
    let t_start = Instant::now();
    let mut report = IndexReport { found: paths.len(), skipped, removed, ..Default::default() };
    if todo.is_empty() {
        report.seconds = t_start.elapsed().as_secs_f64();
        return Ok(report);
    }

    let counter = Arc::new(AtomicU64::new(0));
    let queue: Arc<Mutex<VecDeque<(i64, PathBuf, String)>>> =
        Arc::new(Mutex::new(todo.iter().map(|(pid, p, _)| (*pid, p.clone(), rel_path(lib, p))).collect()));
    let (tx, rx) = mpsc::channel::<Msg>();
    let scan_opts = ScanOptions { keep_previews: opts.previews, ..Default::default() };
    let mut handles = Vec::new();
    for _ in 0..opts.workers.clamp(1, n_todo.max(1)) {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        let counter = Arc::clone(&counter);
        handles.push(std::thread::spawn(move || loop {
            let job = queue.lock().ok().and_then(|mut q| q.pop_front());
            let Some((pid, path, rel)) = job else { break };
            let t0 = Instant::now();
            let mut entries: Vec<Entry> = Vec::new();
            let mut previews: Vec<(String, Vec<u8>)> = Vec::new();
            let mut preview_bytes = 0usize;
            let res = scan_package(&path, scan_opts, Some(Arc::clone(&counter)), |mut e| {
                if let Some(png) = e.preview.take() {
                    preview_bytes += png.len();
                    previews.push((e.guid.clone(), png));
                    if preview_bytes >= PREVIEW_CHUNK_BYTES {
                        let _ = tx.send(Msg::Previews(std::mem::take(&mut previews)));
                        preview_bytes = 0;
                    }
                }
                entries.push(e);
                Ok(())
            });
            if !previews.is_empty() {
                let _ = tx.send(Msg::Previews(previews));
            }
            let error = res.err().map(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Done { pid, rel, entries, seconds: t0.elapsed().as_secs_f64(), error });
        }));
    }
    drop(tx);

    let mut done = 0usize;
    let mut last_report = Instant::now() - Duration::from_secs(10);
    let mut finished = false;
    while !finished {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Msg::Previews(items)) => {
                report.previews += db.put_previews(&items)?;
            }
            Ok(Msg::Done { pid, rel, entries, seconds, error }) => {
                done += 1;
                if let Some(err) = error {
                    report.errors += 1;
                    db.mark_package(pid, "error", Some(&err))?;
                    log(&format!("[{done}/{n_todo}] ERROR {rel}: {}", err.lines().next().unwrap_or("")));
                } else {
                    let (n, total) = db.replace_package_assets(pid, &entries, opts.previews)?;
                    report.entries += n;
                    log(&format!(
                        "[{done}/{n_todo}] {rel}  {n} assets, {:.0} MB uncompressed, {seconds:.0}s",
                        total as f64 / 1e6
                    ));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => finished = true,
        }
        if let Some(p) = progress.as_deref_mut() {
            if last_report.elapsed() >= Duration::from_secs(5) || finished {
                last_report = Instant::now();
                let read = counter.load(Ordering::Relaxed);
                let elapsed = t_start.elapsed().as_secs_f64();
                let rate = if elapsed > 0.0 { read as f64 / elapsed } else { 0.0 };
                let eta = if rate > 0.0 { (total_bytes.saturating_sub(read)) as f64 / rate } else { 0.0 };
                p(&format!(
                    "{done}/{n_todo} packages, {}/{} read, {}/s, ETA {}",
                    fmt_bytes(read as f64),
                    fmt_bytes(total_bytes as f64),
                    fmt_bytes(rate),
                    fmt_eta(eta)
                ));
            }
        }
    }
    for h in handles {
        let _ = h.join();
    }
    report.indexed = n_todo - report.errors;
    report.seconds = t_start.elapsed().as_secs_f64();
    Ok(report)
}
