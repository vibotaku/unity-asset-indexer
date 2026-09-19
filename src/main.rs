//! `uai` command line.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use uai::config::Config;
use uai::error::describe;
use uai::human_bytes as human;
use uai::indexer::{index_library, IndexOptions};
use uai::model::*;
use uai::service::{open_service, ExportDest, ExportOptions, LocalService, Service};

const IDENT_HELP: &str = "guid | Package::Assets/path | Assets/path | path suffix | file name";

#[derive(Parser)]
#[command(
    name = "uai",
    version,
    about = "Search, preview and extract individual assets from a library of .unitypackage files."
)]
struct Cli {
    /// Library root; repeat for several roots (default: config / $UAI_LIBRARY, path-list separated)
    #[arg(long, global = true, action = clap::ArgAction::Append)]
    library: Vec<String>,
    /// Index/cache dir (default: ~/.unity-asset-index or $UAI_HOME)
    #[arg(long, global = true)]
    home: Option<String>,
    /// Run against a remote `uai serve` instance instead of the local index (or $UAI_SERVER / config)
    #[arg(long, global = true)]
    server: Option<String>,
    /// Ignore any configured server and use the local index
    #[arg(long, global = true)]
    local: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Default)]
struct JsonFlag {
    /// Machine-readable output
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show settings and index stats
    Config {
        /// Persist a single library root (replaces the list)
        #[arg(long = "set-library", value_name = "DIR")]
        set_library: Option<String>,
        /// Add a library root to the list
        #[arg(long = "add-library", value_name = "DIR", action = clap::ArgAction::Append)]
        add_library: Vec<String>,
        /// Remove a library root from the list
        #[arg(long = "remove-library", value_name = "DIR", action = clap::ArgAction::Append)]
        remove_library: Vec<String>,
        /// Persist a default server URL for remote mode
        #[arg(long = "set-server", value_name = "URL", conflicts_with = "clear_server")]
        set_server: Option<String>,
        /// Forget the configured server
        #[arg(long)]
        clear_server: bool,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// (Re)index the library; incremental by size+mtime
    Index {
        #[arg(long)]
        workers: Option<usize>,
        /// Re-scan packages even if unchanged
        #[arg(long)]
        force: bool,
        /// Only packages whose relative path contains this substring
        #[arg(long)]
        only: Option<String>,
        /// Do not store preview thumbnails
        #[arg(long)]
        no_previews: bool,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// List indexed packages
    Packages {
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Full-text search over asset paths
    Search {
        /// Words (prefix-matched; camelCase and _ split)
        query: Vec<String>,
        /// Comma list of kinds: prefab material shader texture model audio animation scene script asset font ui video doc data vfx
        #[arg(short, long)]
        kind: Option<String>,
        /// File extension, e.g. fbx
        #[arg(short, long)]
        ext: Option<String>,
        /// Restrict to packages whose name contains this
        #[arg(short, long)]
        package: Option<String>,
        #[arg(short = 'P', long)]
        publisher: Option<String>,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Include folder entries
        #[arg(long)]
        folders: bool,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// List a package's contents
    Ls {
        /// Package id or (partial) name
        package: String,
        /// Only under this Assets/... prefix
        #[arg(long)]
        path: Option<String>,
        #[arg(short, long)]
        kind: Option<String>,
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        #[arg(long)]
        tree: bool,
        #[arg(long)]
        folders: bool,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Details, direct dependencies and users of one asset
    Info {
        #[arg(help = IDENT_HELP)]
        asset: String,
        #[arg(short, long)]
        package: Option<String>,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Transitive dependency closure (cross-package)
    Deps {
        #[arg(required = true, help = IDENT_HELP)]
        assets: Vec<String>,
        #[arg(short, long)]
        package: Option<String>,
        /// Limit traversal depth
        #[arg(long)]
        depth: Option<u32>,
        /// Do not follow into scripts/plugins
        #[arg(long)]
        no_scripts: bool,
        /// Also print as a tree
        #[arg(long)]
        tree: bool,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Assets that reference this asset
    Rdeps {
        #[arg(help = IDENT_HELP)]
        asset: String,
        #[arg(short, long)]
        package: Option<String>,
        #[arg(short = 'n', long, default_value_t = 200)]
        limit: usize,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Extract assets + dependencies into a Unity project, folder or .unitypackage
    Export {
        #[arg(required = true, help = IDENT_HELP)]
        assets: Vec<String>,
        #[arg(short, long)]
        package: Option<String>,
        /// Unity project dir (writes Assets/... with .meta files)
        #[arg(long)]
        project: Option<PathBuf>,
        /// Plain output dir
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write a slim .unitypackage instead
        #[arg(long)]
        unitypackage: Option<PathBuf>,
        #[arg(long)]
        no_deps: bool,
        /// Leave scripts/plugins out of the dependency set
        #[arg(long)]
        no_scripts: bool,
        /// Do not write folder .meta files
        #[arg(long)]
        no_folders: bool,
        /// Skip scanning project .meta guids
        #[arg(long)]
        no_conflict_check: bool,
        /// Overwrite existing files
        #[arg(long)]
        force: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(short, long)]
        quiet: bool,
        #[arg(short, long)]
        verbose: bool,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Extract an asset's preview.png
    Preview {
        #[arg(help = IDENT_HELP)]
        asset: String,
        #[arg(short, long)]
        package: Option<String>,
        #[arg(short, long)]
        out: Option<PathBuf>,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Print a text asset (prefab YAML, script, shader...)
    Cat {
        #[arg(help = IDENT_HELP)]
        asset: String,
        #[arg(short, long)]
        package: Option<String>,
        #[arg(long, default_value_t = 2_000_000)]
        max_bytes: usize,
    },
    /// Keep a fully-extracted local copy of a package for fast exports
    Cache {
        #[arg(value_parser = ["add", "rm", "ls"])]
        action: String,
        package: Option<String>,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Run the index server with the web UI
    Serve {
        /// Address to listen on
        #[arg(long, default_value = "127.0.0.1:7878")]
        bind: String,
        /// Open the UI in a browser once the server is up
        #[arg(long)]
        open: bool,
    },
    /// Run the MCP server (stdio) for agents
    Mcp,
}

fn out_json<T: Serialize>(v: &T) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, v)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(()) => 0,
        Err(e) => {
            if let Some(ioe) = e.downcast_ref::<std::io::Error>() {
                if ioe.kind() == std::io::ErrorKind::BrokenPipe {
                    std::process::exit(0);
                }
            }
            eprintln!("error: {}", describe(&e));
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<()> {
    let mut cfg = Config::load(&cli.library, cli.home.as_deref(), cli.server.as_deref())?;
    if cli.local {
        cfg.server = None;
    }
    match cli.cmd {
        Cmd::Config { set_library, add_library, remove_library, set_server, clear_server, json } => {
            cmd_config(cfg, set_library, add_library, remove_library, set_server, clear_server, json.json)
        }
        Cmd::Index { workers, force, only, no_previews, json } => {
            cmd_index(&cfg, workers, force, only, !no_previews, json.json)
        }
        Cmd::Packages { json } => cmd_packages(&*open_service(&cfg)?, json.json),
        Cmd::Search { query, kind, ext, package, publisher, limit, folders, json } => {
            let q = SearchQuery {
                query: query.join(" "),
                kind,
                package,
                publisher,
                ext,
                include_folders: folders,
                limit,
                offset: 0,
            };
            cmd_search(&*open_service(&cfg)?, &q, json.json)
        }
        Cmd::Ls { package, path, kind, limit, tree, folders, json } => {
            cmd_ls(&*open_service(&cfg)?, &package, path.as_deref(), kind.as_deref(), limit, tree, folders, json.json)
        }
        Cmd::Info { asset, package, json } => cmd_info(&*open_service(&cfg)?, &asset, package.as_deref(), json.json),
        Cmd::Deps { assets, package, depth, no_scripts, tree, json } => {
            cmd_deps(&*open_service(&cfg)?, &assets, package.as_deref(), depth, !no_scripts, tree, json.json)
        }
        Cmd::Rdeps { asset, package, limit, json } => {
            cmd_rdeps(&*open_service(&cfg)?, &asset, package.as_deref(), limit, json.json)
        }
        Cmd::Export {
            assets,
            package,
            project,
            out,
            unitypackage,
            no_deps,
            no_scripts,
            no_folders,
            no_conflict_check,
            force,
            dry_run,
            quiet,
            verbose,
            json,
        } => {
            let dests = [project.is_some(), out.is_some(), unitypackage.is_some()].iter().filter(|b| **b).count();
            if dests != 1 {
                bail!("choose exactly one destination: --project <UnityProject> | --out <dir> | --unitypackage <file>");
            }
            let dest = if let Some(p) = project {
                ExportDest::Project(p)
            } else if let Some(o) = out {
                ExportDest::Dir(o)
            } else {
                ExportDest::UnityPackage(unitypackage.unwrap())
            };
            let req = ExportRequest {
                identifiers: assets,
                package,
                include_deps: !no_deps,
                include_scripts: !no_scripts,
                include_folders: !no_folders,
            };
            let opts = ExportOptions { force, dry_run, conflict_check: !no_conflict_check };
            cmd_export(&*open_service(&cfg)?, &req, &dest, &opts, quiet, verbose, json.json)
        }
        Cmd::Preview { asset, package, out, json } => {
            cmd_preview(&*open_service(&cfg)?, &asset, package.as_deref(), out, json.json)
        }
        Cmd::Cat { asset, package, max_bytes } => {
            let t = open_service(&cfg)?.text(&asset, package.as_deref(), max_bytes)?;
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(t.text.as_bytes())?;
            if !t.text.ends_with('\n') {
                stdout.write_all(b"\n")?;
            }
            if t.truncated {
                eprintln!("(truncated at {max_bytes} bytes; use --max-bytes)");
            }
            Ok(())
        }
        Cmd::Cache { action, package, json } => cmd_cache(&cfg, &action, package.as_deref(), json.json),
        Cmd::Serve { bind, open } => uai::server::serve(cfg, &bind, open),
        Cmd::Mcp => uai::mcp::run(cfg),
    }
}

// ----- commands -------------------------------------------------------------------------------------

fn cmd_config(
    mut cfg: Config,
    set_library: Option<String>,
    add_library: Vec<String>,
    remove_library: Vec<String>,
    set_server: Option<String>,
    clear_server: bool,
    json: bool,
) -> Result<()> {
    if let Some(l) = set_library {
        cfg.save_library(&l)?;
    }
    for l in &add_library {
        cfg.add_library(l)?;
    }
    for l in &remove_library {
        if !cfg.remove_library(l)? {
            eprintln!("note: {l} was not in the library list");
        }
    }
    if let Some(s) = set_server {
        cfg.save_server(Some(&s))?;
    }
    if clear_server {
        cfg.save_server(None)?;
    }
    let mut info: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    info.insert("library", cfg.libraries_display().into());
    info.insert(
        "libraries",
        serde_json::Value::Array(
            cfg.libraries
                .iter()
                .map(|p| serde_json::json!({"path": p.to_string_lossy(), "mounted": p.is_dir()}))
                .collect(),
        ),
    );
    info.insert("library_mounted", cfg.library_mounted().into());
    info.insert("home", cfg.home.to_string_lossy().to_string().into());
    info.insert("db", cfg.db_path().to_string_lossy().to_string().into());
    info.insert("previews_db", cfg.previews_path().to_string_lossy().to_string().into());
    info.insert("cache_dir", cfg.cache_dir().to_string_lossy().to_string().into());
    info.insert("server", cfg.server.clone().map(Into::into).unwrap_or(serde_json::Value::Null));
    info.insert("version", uai::VERSION.into());
    let stats: Result<Stats> = (|| {
        let svc = open_service(&cfg)?;
        svc.stats()
    })();
    match stats {
        Ok(s) => {
            info.insert("packages", s.packages.into());
            info.insert("assets", s.assets.into());
            info.insert("refs", s.refs.into());
            info.insert("bytes", s.bytes.into());
            info.insert("previews", s.previews.into());
            info.insert("fts", s.fts.into());
        }
        Err(e) => {
            info.insert("stats_error", format!("{e:#}").into());
        }
    }
    if json {
        return out_json(&info);
    }
    for (k, v) in &info {
        if *k == "library" {
            continue;
        }
        let s = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => "-".into(),
            serde_json::Value::Array(a) if *k == "libraries" => a
                .iter()
                .map(|e| {
                    let mounted = e["mounted"].as_bool().unwrap_or(false);
                    format!("{}{}", e["path"].as_str().unwrap_or(""), if mounted { "" } else { "  (not mounted)" })
                })
                .collect::<Vec<_>>()
                .join("\n                 "),
            other => other.to_string(),
        };
        println!("{k:16} {s}");
    }
    Ok(())
}

fn cmd_index(
    cfg: &Config,
    workers: Option<usize>,
    force: bool,
    only: Option<String>,
    previews: bool,
    json: bool,
) -> Result<()> {
    if cfg.server.is_some() {
        bail!("`uai index` runs on the machine that has the library; drop --server (or use --local)");
    }
    let mut svc = LocalService::open(cfg)?;
    let mut opts = IndexOptions { force, only, previews, ..Default::default() };
    if let Some(w) = workers {
        opts.workers = w.max(1);
    }
    let is_tty = std::io::stderr().is_terminal();
    let quiet = json;
    let mut log = |s: &str| {
        if quiet {
            return;
        }
        if is_tty {
            eprint!("\r\x1b[K");
        }
        println!("{s}");
    };
    let mut progress = |s: &str| {
        if is_tty {
            eprint!("\r\x1b[K  {s}");
            let _ = std::io::stderr().flush();
        } else {
            println!("  {s}");
        }
    };
    let res = index_library(cfg, &mut svc.db, &opts, &mut log, if quiet { None } else { Some(&mut progress) })?;
    if is_tty && !quiet {
        eprint!("\r\x1b[K");
    }
    if json {
        return out_json(&res);
    }
    println!(
        "done: {} indexed, {} up to date, {} errors, {} assets, {} previews in {:.0}s",
        res.indexed, res.skipped, res.errors, res.entries, res.previews, res.seconds
    );
    Ok(())
}

fn cmd_packages(svc: &dyn Service, json: bool) -> Result<()> {
    let rows = svc.packages()?;
    if json {
        return out_json(&rows);
    }
    let roots: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.root.as_str()).collect();
    let multi_root = roots.len() > 1;
    println!(
        "{:>3}  {:>7}  {:>9}  {:>9}  {:>9}  {:>11}  publisher / name  [category]",
        "id", "assets", "unpacked", "package", "version", "unity"
    );
    for r in &rows {
        let mut flags = String::new();
        if r.status != "ok" {
            flags.push_str(&format!("  [{}]", r.status));
        }
        if r.cached {
            flags.push_str("  [cached]");
        }
        if multi_root {
            let short =
                std::path::Path::new(&r.root).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            flags.push_str(&format!("  @{short}"));
        }
        println!(
            "{:>3}  {:>7}  {:>9}  {:>9}  {:>9}  {:>11}  {} / {}  [{}]{}",
            r.id,
            r.entry_count,
            human(r.total_bytes),
            human(r.size),
            r.version.clone().unwrap_or_default(),
            r.unity_version.clone().unwrap_or_default(),
            r.publisher,
            r.name,
            r.display_category(),
            flags
        );
    }
    Ok(())
}

fn cmd_search(svc: &dyn Service, q: &SearchQuery, json: bool) -> Result<()> {
    let rows = svc.search(q)?;
    if json {
        return out_json(&rows);
    }
    if rows.is_empty() {
        println!("no matches");
        return Ok(());
    }
    for r in &rows {
        println!("{}  {:<9} {:>8}  [{}]  {}", r.guid, r.kind, human(r.size), r.package, r.path);
    }
    println!("-- {} result(s){}", rows.len(), if rows.len() >= q.limit { " (limit reached)" } else { "" });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_ls(
    svc: &dyn Service,
    package: &str,
    path: Option<&str>,
    kind: Option<&str>,
    limit: Option<usize>,
    tree: bool,
    folders: bool,
    json: bool,
) -> Result<()> {
    let listing = svc.ls(package, path, kind, limit)?;
    if json {
        return out_json(&listing);
    }
    let p = &listing.package;
    println!("[{}] {} / {}  ({} assets, {})", p.id, p.publisher, p.name, p.entry_count, human(p.total_bytes));
    for k in &listing.kinds {
        println!("   {:<10} {:>6}  {}", k.kind, k.n, human(k.bytes));
    }
    if tree {
        print_tree(&listing.assets);
    } else {
        for r in &listing.assets {
            if r.is_folder && !folders {
                continue;
            }
            println!("{}  {:<9} {:>8}  {}", r.guid, r.kind, human(r.size), r.path);
        }
    }
    Ok(())
}

#[derive(Default)]
struct TreeNode {
    dirs: BTreeMap<String, TreeNode>,
    files: BTreeMap<String, String>,
}

fn print_tree(rows: &[Asset]) {
    let mut root = TreeNode::default();
    for r in rows {
        let parts: Vec<&str> = r.path.split('/').collect();
        let mut node = &mut root;
        for p in &parts[..parts.len().saturating_sub(1)] {
            node = node.dirs.entry(p.to_string()).or_default();
        }
        if !r.is_folder {
            if let Some(last) = parts.last() {
                node.files.insert(last.to_string(), human(r.size));
            }
        }
    }
    fn walk(node: &TreeNode, indent: usize) {
        for (k, v) in &node.dirs {
            println!("{}{}/", "  ".repeat(indent), k);
            walk(v, indent + 1);
        }
        for (k, v) in &node.files {
            println!("{}{}  ({})", "  ".repeat(indent), k, v);
        }
    }
    walk(&root, 0);
}

fn cmd_info(svc: &dyn Service, ident: &str, package: Option<&str>, json: bool) -> Result<()> {
    let info = svc.info(ident, package)?;
    if json {
        return out_json(&info);
    }
    let a = &info.asset;
    println!("{:12} {}", "guid", a.guid);
    println!("{:12} {}", "path", a.path);
    println!("{:12} {}", "kind", a.kind);
    println!("{:12} {}", "ext", a.ext);
    println!("{:12} {}", "importer", a.importer.clone().unwrap_or_default());
    println!("{:12} {}", "main_class", a.main_class.clone().unwrap_or_default());
    println!("{:12} {}", "package", a.package);
    println!("{:12} {}", "publisher", a.publisher);
    println!(
        "{:12} {}   preview: {}   text: {}",
        "size",
        human(a.size),
        if a.has_preview { "yes" } else { "no" },
        if a.is_text { "yes" } else { "no" }
    );
    if !a.labels.is_empty() {
        println!("{:12} {}", "labels", a.labels.join(", "));
    }
    if !info.same_guid_in.is_empty() {
        println!(
            "{:12} {}",
            "also in",
            info.same_guid_in.iter().map(|o| o.package.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    println!("{:12} {}", "direct deps", info.refs.len());
    for r in info.refs.iter().take(50) {
        match &r.asset {
            Some(c) => println!("   {}  {:<9} [{}] {}", r.guid, c.kind, c.package, c.path),
            None => println!("   {}  (unresolved) {}", r.guid, r.label.clone().unwrap_or_default()),
        }
    }
    if info.refs.len() > 50 {
        println!("   ... {} more", info.refs.len() - 50);
    }
    println!(
        "{:12} {}{}",
        "used by",
        info.referrers.len(),
        if info.referrers.len() > 20 { " (showing 20)" } else { "" }
    );
    for r in info.referrers.iter().take(20) {
        println!("   {}  {:<9} [{}] {}", r.guid, r.kind, r.package, r.path);
    }
    Ok(())
}

fn cmd_deps(
    svc: &dyn Service,
    idents: &[String],
    package: Option<&str>,
    depth: Option<u32>,
    include_scripts: bool,
    tree: bool,
    json: bool,
) -> Result<()> {
    let cl = svc.deps(idents, package, include_scripts, depth)?;
    if json {
        return out_json(&cl);
    }
    println!(
        "{} asset(s) in closure, {} total, {} unresolved, {} scripts skipped",
        cl.assets.len(),
        human(cl.total_bytes),
        cl.unresolved.len(),
        cl.skipped_scripts.len()
    );
    if tree {
        print_dep_tree(&cl, depth.unwrap_or(6) as usize);
    }
    let mut by_pkg: BTreeMap<i64, Vec<&ClosureAsset>> = BTreeMap::new();
    for a in &cl.assets {
        by_pkg.entry(a.asset.package_id).or_default().push(a);
    }
    for (pid, nodes) in &by_pkg {
        let name = cl.packages.iter().find(|p| p.id == *pid).map(|p| p.name.as_str()).unwrap_or("?");
        let publisher = nodes.first().map(|n| n.asset.publisher.as_str()).unwrap_or("");
        let bytes: i64 = nodes.iter().map(|n| n.asset.size).sum();
        println!("\n[{pid}] {publisher} / {name}  ({} assets, {})", nodes.len(), human(bytes));
        for n in nodes {
            let tag = if n.depth == 0 { "root".to_string() } else { format!("d{}", n.depth) };
            println!("  {}  {:<4} {:<9} {:>8}  {}", n.asset.guid, tag, n.asset.kind, human(n.asset.size), n.asset.path);
        }
    }
    if !cl.skipped_scripts.is_empty() {
        println!("\nscripts skipped (--no-scripts):");
        for s in &cl.skipped_scripts {
            println!("  {}  [{}] {}", s.guid, s.package, s.path);
        }
    }
    if !cl.unresolved.is_empty() {
        println!("\nunresolved guids (not in library; Unity built-ins / UPM packages / packages you do not have):");
        for u in &cl.unresolved {
            let first = u.referrers.first().cloned().unwrap_or_default();
            let via = cl
                .assets
                .iter()
                .find(|a| a.asset.guid == first)
                .map(|a| a.asset.path.rsplit('/').next().unwrap_or("").to_string())
                .unwrap_or(first);
            let more = if u.referrers.len() > 1 { format!(" +{}", u.referrers.len() - 1) } else { String::new() };
            println!("  {}  {}   (referenced by {via}{more})", u.guid, u.label.clone().unwrap_or_else(|| "?".into()));
        }
    }
    Ok(())
}

fn print_dep_tree(cl: &ClosureOut, depth: usize) {
    let by_guid: BTreeMap<&str, &ClosureAsset> = cl.assets.iter().map(|a| (a.asset.guid.as_str(), a)).collect();
    let skipped: BTreeMap<&str, &Asset> = cl.skipped_scripts.iter().map(|a| (a.guid.as_str(), a)).collect();
    let unresolved: BTreeMap<&str, &Unresolved> = cl.unresolved.iter().map(|u| (u.guid.as_str(), u)).collect();
    let mut printed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    #[allow(clippy::too_many_arguments)]
    fn walk<'a>(
        guid: &'a str,
        indent: usize,
        depth: usize,
        cl: &'a ClosureOut,
        by_guid: &BTreeMap<&'a str, &'a ClosureAsset>,
        skipped: &BTreeMap<&'a str, &'a Asset>,
        unresolved: &BTreeMap<&'a str, &'a Unresolved>,
        printed: &mut std::collections::HashSet<&'a str>,
    ) {
        let pad = "  ".repeat(indent);
        let Some(node) = by_guid.get(guid) else {
            let label = unresolved.get(guid).and_then(|u| u.label.clone()).unwrap_or_else(|| "unresolved".into());
            println!("{pad}?? {guid}  ({label})");
            return;
        };
        let dup = if printed.contains(guid) { " (see above)" } else { "" };
        println!("{pad}{:<9} {}{dup}", node.asset.kind, node.asset.path);
        if !dup.is_empty() || indent >= depth {
            return;
        }
        printed.insert(guid);
        if let Some(children) = cl.edges.get(guid) {
            for c in children {
                if let Some(s) = skipped.get(c.as_str()) {
                    println!("{}script    {} (skipped)", "  ".repeat(indent + 1), s.path);
                    continue;
                }
                walk(c, indent + 1, depth, cl, by_guid, skipped, unresolved, printed);
            }
        }
    }
    for r in &cl.roots {
        walk(r, 0, depth, cl, &by_guid, &skipped, &unresolved, &mut printed);
    }
    println!();
}

fn cmd_rdeps(svc: &dyn Service, ident: &str, package: Option<&str>, limit: usize, json: bool) -> Result<()> {
    let asset = svc.resolve(ident, package)?;
    let rows = svc.rdeps(ident, package, Some(limit))?;
    if json {
        return out_json(&rows);
    }
    println!("{} asset(s) reference {}", rows.len(), asset.path);
    for r in &rows {
        println!("  {}  {:<9} [{}] {}", r.guid, r.kind, r.package, r.path);
    }
    Ok(())
}

fn cmd_export(
    svc: &dyn Service,
    req: &ExportRequest,
    dest: &ExportDest,
    opts: &ExportOptions,
    quiet: bool,
    verbose: bool,
    json: bool,
) -> Result<()> {
    let mut log = |s: &str| {
        if !json && !quiet {
            eprintln!("{s}");
        }
    };
    let out = svc.export(req, dest, opts, &mut log)?;
    if json {
        return out_json(&out);
    }
    let res = &out.result;
    let verb = if opts.dry_run { "would write" } else { "wrote" };
    println!(
        "{verb} {} file(s) ({}) to {} in {:.1}s",
        res.written.len(),
        human(res.bytes_written),
        res.output,
        res.seconds
    );
    if !quiet {
        let show = if verbose { res.written.len() } else { 40 };
        for w in res.written.iter().take(show) {
            println!("  + {}", w.path);
        }
        if res.written.len() > 40 && !verbose {
            println!("  ... {} more (use -v)", res.written.len() - 40);
        }
    }
    if !res.skipped_existing.is_empty() {
        println!("skipped {} existing file(s) (use --force to overwrite)", res.skipped_existing.len());
    }
    if !res.conflicts.is_empty() {
        println!("CONFLICT: {} guid(s) already exist in the project at a different path:", res.conflicts.len());
        for c in &res.conflicts {
            println!("  {}  {}  ->  already at {}", c.guid, c.path, c.existing_path);
        }
    }
    if !res.missing_in_package.is_empty() {
        println!(
            "WARNING: {} planned asset(s) were not found inside the package (index stale? run `uai index`)",
            res.missing_in_package.len()
        );
    }
    for w in &out.plan.warnings {
        println!("note: {w}");
    }
    if !out.plan.unresolved.is_empty() && !quiet {
        println!("unresolved references:");
        for u in out.plan.unresolved.iter().take(20) {
            println!("  {}  {}", u.guid, u.label.clone().unwrap_or_else(|| "?".into()));
        }
        if out.plan.unresolved.len() > 20 {
            println!("  ... {} more (see `uai deps`)", out.plan.unresolved.len() - 20);
        }
    }
    Ok(())
}

fn cmd_preview(svc: &dyn Service, ident: &str, package: Option<&str>, out: Option<PathBuf>, json: bool) -> Result<()> {
    let asset = svc.resolve(ident, package)?;
    let png = svc.preview(ident, package)?;
    let out = out.unwrap_or_else(|| PathBuf::from(format!("{}.preview.png", asset.name)));
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, &png)?;
    if json {
        return out_json(&serde_json::json!({ "path": out.to_string_lossy(), "bytes": png.len() }));
    }
    println!("{}", out.display());
    Ok(())
}

fn cmd_cache(cfg: &Config, action: &str, package: Option<&str>, json: bool) -> Result<()> {
    if cfg.server.is_some() {
        bail!("the cache lives on the machine with the library; drop --server (or use --local)");
    }
    let svc = LocalService::open(cfg)?;
    if action == "ls" {
        let rows = svc.cache_list()?;
        if json {
            return out_json(&rows);
        }
        for r in &rows {
            println!(
                "[{}] {:<50} {:>9}  {}",
                r.package_id,
                r.package,
                human(r.bytes),
                if r.complete { "ok" } else { "partial" }
            );
        }
        if rows.is_empty() {
            println!("cache is empty");
        }
        return Ok(());
    }
    let Some(package) = package else { bail!("package required") };
    let pkg = svc.find_package(package)?;
    match action {
        "add" => {
            let mut log = |s: &str| println!("{s}");
            let p = uai::exporter::cache_add(&svc.cfg, &pkg, &mut log)?;
            if json {
                return out_json(&serde_json::json!({ "path": p.to_string_lossy() }));
            }
            println!("{}", p.display());
        }
        "rm" => {
            let removed = uai::exporter::cache_remove(&svc.cfg, pkg.id)?;
            if json {
                return out_json(&serde_json::json!({ "removed": removed }));
            }
            println!("{}", if removed { "removed" } else { "not cached" });
        }
        _ => unreachable!(),
    }
    Ok(())
}
