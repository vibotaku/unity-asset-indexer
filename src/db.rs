//! SQLite storage for the asset index (FTS5 full-text search) plus the preview thumbnail store.
//!
//! The schema is compatible with the original Python `uai` index (schema version 1); version 2 adds
//! `packages.previews_indexed` and the separate `previews.db` (attached as `pv`); version 3 adds
//! `packages.root` (several library roots) with `UNIQUE(root, rel_path)`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};

use crate::model::{Asset, KindCount, Package, SearchQuery, Stats};
use crate::unitypackage::Entry;

pub const SCHEMA_VERSION: i64 = 3;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS packages (
    id INTEGER PRIMARY KEY,
    root TEXT NOT NULL DEFAULT '',
    rel_path TEXT NOT NULL,
    name TEXT NOT NULL,
    publisher TEXT,
    category TEXT,
    size INTEGER,
    mtime REAL,
    indexed_at REAL,
    entry_count INTEGER DEFAULT 0,
    total_bytes INTEGER DEFAULT 0,
    status TEXT DEFAULT 'ok',
    error TEXT,
    title TEXT,
    version TEXT,
    unity_version TEXT,
    pubdate TEXT,
    store_id TEXT,
    category_label TEXT,
    description TEXT,
    header TEXT,
    previews_indexed INTEGER DEFAULT 0,
    UNIQUE(root, rel_path)
);
CREATE TABLE IF NOT EXISTS assets (
    id INTEGER PRIMARY KEY,
    package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
    guid TEXT NOT NULL,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    ext TEXT,
    kind TEXT,
    importer TEXT,
    main_class TEXT,
    size INTEGER DEFAULT 0,
    is_folder INTEGER DEFAULT 0,
    has_preview INTEGER DEFAULT 0,
    is_text INTEGER DEFAULT 0,
    scan_truncated INTEGER DEFAULT 0,
    labels TEXT,
    UNIQUE(package_id, guid)
);
CREATE INDEX IF NOT EXISTS idx_assets_guid ON assets(guid);
CREATE INDEX IF NOT EXISTS idx_assets_path ON assets(path);
CREATE INDEX IF NOT EXISTS idx_assets_name ON assets(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_assets_kind ON assets(kind);
CREATE TABLE IF NOT EXISTS refs (
    asset_id INTEGER NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    dep_guid TEXT NOT NULL,
    PRIMARY KEY (asset_id, dep_guid)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_refs_dep ON refs(dep_guid);
"#;

const FTS_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS assets_fts USING fts5(
    asset_id UNINDEXED, name, path, tokens, package, publisher, labels,
    tokenize = 'unicode61'
);
"#;

const PREVIEW_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS pv.previews (
    guid TEXT PRIMARY KEY,
    png BLOB NOT NULL,
    size INTEGER NOT NULL
) WITHOUT ROWID;
"#;

const ASSET_COLS: &str =
    "a.id, a.package_id, a.guid, a.path, a.name, a.ext, a.kind, a.importer, a.main_class, a.size, \
     a.is_folder, a.has_preview, a.is_text, a.scan_truncated, a.labels, \
     p.name AS package, p.publisher AS publisher, p.rel_path AS package_rel_path";

pub struct Database {
    pub conn: Connection,
    pub path: PathBuf,
    pub has_fts: bool,
}

/// Split CamelCase / digits / separators into words, the same way for indexing and querying.
pub fn camel_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if matches!(c, '_' | '-' | '.' | '/' | '\\') {
            out.push(' ');
            continue;
        }
        if i > 0 {
            let p = chars[i - 1];
            let boundary = (p.is_ascii_lowercase() || p.is_ascii_digit()) && c.is_ascii_uppercase()
                || p.is_ascii_alphabetic() && c.is_ascii_digit()
                || p.is_ascii_digit() && c.is_ascii_alphabetic();
            if boundary {
                out.push(' ');
            }
        }
        out.push(c);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn tokens_for(path: &str, kind: &str, ext: &str, main_class: Option<&str>) -> String {
    let mut parts = vec![camel_tokens(path)];
    for p in [kind, ext, main_class.unwrap_or("")] {
        if !p.is_empty() {
            parts.push(p.to_string());
        }
    }
    parts.join(" ")
}

fn asset_from_row(r: &Row<'_>) -> rusqlite::Result<Asset> {
    let labels: Option<String> = r.get("labels")?;
    Ok(Asset {
        id: r.get("id")?,
        package_id: r.get("package_id")?,
        guid: r.get("guid")?,
        path: r.get("path")?,
        name: r.get("name")?,
        ext: r.get::<_, Option<String>>("ext")?.unwrap_or_default(),
        kind: r.get::<_, Option<String>>("kind")?.unwrap_or_default(),
        importer: r.get("importer")?,
        main_class: r.get("main_class")?,
        size: r.get::<_, Option<i64>>("size")?.unwrap_or(0),
        is_folder: r.get::<_, Option<i64>>("is_folder")?.unwrap_or(0) != 0,
        has_preview: r.get::<_, Option<i64>>("has_preview")?.unwrap_or(0) != 0,
        is_text: r.get::<_, Option<i64>>("is_text")?.unwrap_or(0) != 0,
        scan_truncated: r.get::<_, Option<i64>>("scan_truncated")?.unwrap_or(0) != 0,
        labels: labels.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
        package: r.get::<_, Option<String>>("package")?.unwrap_or_default(),
        publisher: r.get::<_, Option<String>>("publisher")?.unwrap_or_default(),
        package_rel_path: r.get::<_, Option<String>>("package_rel_path")?.unwrap_or_default(),
    })
}

fn package_from_row(r: &Row<'_>) -> rusqlite::Result<Package> {
    Ok(Package {
        id: r.get("id")?,
        root: r.get::<_, Option<String>>("root")?.unwrap_or_default(),
        rel_path: r.get("rel_path")?,
        name: r.get("name")?,
        publisher: r.get::<_, Option<String>>("publisher")?.unwrap_or_default(),
        category: r.get::<_, Option<String>>("category")?.unwrap_or_default(),
        size: r.get::<_, Option<i64>>("size")?.unwrap_or(0),
        mtime: r.get::<_, Option<f64>>("mtime")?.unwrap_or(0.0),
        indexed_at: r.get("indexed_at")?,
        entry_count: r.get::<_, Option<i64>>("entry_count")?.unwrap_or(0),
        total_bytes: r.get::<_, Option<i64>>("total_bytes")?.unwrap_or(0),
        status: r.get::<_, Option<String>>("status")?.unwrap_or_else(|| "ok".into()),
        error: r.get("error")?,
        title: r.get("title")?,
        version: r.get("version")?,
        unity_version: r.get("unity_version")?,
        pubdate: r.get("pubdate")?,
        store_id: r.get("store_id")?,
        category_label: r.get("category_label")?,
        description: r.get("description")?,
        previews_indexed: r.get::<_, Option<i64>>("previews_indexed")?.unwrap_or(0) != 0,
        cached: false,
    })
}

impl Database {
    /// Open (or create) the index at `path`; the preview store lives next to it as `previews.db`.
    pub fn open(path: &Path) -> Result<Database> {
        Self::open_with_previews(path, &path.with_file_name("previews.db"))
    }

    pub fn open_with_previews(path: &Path, previews: &Path) -> Result<Database> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(60))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::migrate(&conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA).context("creating schema")?;
        let has_fts = conn.execute_batch(FTS_SCHEMA).is_ok();
        conn.execute("ATTACH DATABASE ?1 AS pv", params![previews.to_string_lossy().to_string()])
            .with_context(|| format!("attaching {}", previews.display()))?;
        conn.execute_batch(PREVIEW_SCHEMA).context("creating preview schema")?;
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(Database { conn, path: path.to_path_buf(), has_fts })
    }

    /// Bring an older index up to the current schema (runs before `foreign_keys` is switched on, so
    /// rebuilding the packages table does not cascade into assets).
    fn migrate(conn: &Connection) -> Result<()> {
        let has_table: bool =
            conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='packages'")?.exists([])?;
        if !has_table {
            return Ok(());
        }
        let has_col = |name: &str| -> rusqlite::Result<bool> {
            conn.prepare(&format!("SELECT 1 FROM pragma_table_info('packages') WHERE name='{name}'"))?.exists([])
        };
        if !has_col("previews_indexed")? {
            conn.execute_batch("ALTER TABLE packages ADD COLUMN previews_indexed INTEGER DEFAULT 0")?;
        }
        if !has_col("root")? {
            // v2 -> v3: rebuild with `root` and UNIQUE(root, rel_path). Ids are preserved so the
            // assets.package_id references stay valid. Foreign keys MUST be off here: the bundled
            // SQLite defaults them to on, and `DROP TABLE packages` would otherwise cascade into
            // `assets` (ON DELETE CASCADE) and wipe the index.
            conn.pragma_update(None, "foreign_keys", "OFF")?;
            let assets_before: i64 = conn.query_row("SELECT COUNT(*) FROM assets", [], |r| r.get(0)).unwrap_or(0);
            conn.execute_batch(
                r#"BEGIN;
                CREATE TABLE packages_v3 (
                    id INTEGER PRIMARY KEY, root TEXT NOT NULL DEFAULT '', rel_path TEXT NOT NULL, name TEXT NOT NULL,
                    publisher TEXT, category TEXT, size INTEGER, mtime REAL, indexed_at REAL, entry_count INTEGER DEFAULT 0,
                    total_bytes INTEGER DEFAULT 0, status TEXT DEFAULT 'ok', error TEXT, title TEXT, version TEXT,
                    unity_version TEXT, pubdate TEXT, store_id TEXT, category_label TEXT, description TEXT, header TEXT,
                    previews_indexed INTEGER DEFAULT 0, UNIQUE(root, rel_path));
                INSERT INTO packages_v3 (id, root, rel_path, name, publisher, category, size, mtime, indexed_at, entry_count,
                    total_bytes, status, error, title, version, unity_version, pubdate, store_id, category_label, description,
                    header, previews_indexed)
                  SELECT id, '', rel_path, name, publisher, category, size, mtime, indexed_at, entry_count, total_bytes,
                    status, error, title, version, unity_version, pubdate, store_id, category_label, description, header,
                    previews_indexed FROM packages;
                DROP TABLE packages;
                ALTER TABLE packages_v3 RENAME TO packages;
                COMMIT;"#,
            )
            .context("migrating packages table to schema v3")?;
            let assets_after: i64 = conn.query_row("SELECT COUNT(*) FROM assets", [], |r| r.get(0)).unwrap_or(0);
            if assets_after != assets_before {
                anyhow::bail!(
                    "schema migration lost asset rows ({assets_before} -> {assets_after}); run `uai index --force`"
                );
            }
        }
        Ok(())
    }

    // ----- packages -----------------------------------------------------------------------------

    /// Packages indexed before roots were recorded (root = '') are assigned to `primary`.
    pub fn assign_default_root(&self, primary: &str) -> Result<usize> {
        if primary.is_empty() {
            return Ok(0);
        }
        Ok(self.conn.execute("UPDATE packages SET root=?1 WHERE root=''", params![primary])?)
    }

    pub fn package_by_root_rel(&self, root: &str, rel_path: &str) -> Result<Option<Package>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM packages WHERE root=?1 AND rel_path=?2",
                params![root, rel_path],
                package_from_row,
            )
            .optional()?)
    }

    pub fn asset_count(&self, pid: i64) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM assets WHERE package_id=?1", params![pid], |r| r.get(0))?)
    }

    pub fn package_by_id(&self, pid: i64) -> Result<Option<Package>> {
        Ok(self.conn.query_row("SELECT * FROM packages WHERE id=?1", params![pid], package_from_row).optional()?)
    }

    pub fn packages(&self) -> Result<Vec<Package>> {
        let mut st =
            self.conn.prepare("SELECT * FROM packages ORDER BY publisher COLLATE NOCASE, name COLLATE NOCASE")?;
        let rows = st.query_map([], package_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Match by exact id, exact name, or case-insensitive substring of name/rel_path.
    pub fn find_packages(&self, needle: &str) -> Result<Vec<Package>> {
        let needle = needle.trim();
        if !needle.is_empty() && needle.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(self.package_by_id(needle.parse()?)?.into_iter().collect());
        }
        let mut st = self.conn.prepare("SELECT * FROM packages WHERE name = ?1 COLLATE NOCASE")?;
        let exact: Vec<Package> = st.query_map(params![needle], package_from_row)?.collect::<rusqlite::Result<_>>()?;
        if !exact.is_empty() {
            return Ok(exact);
        }
        let like = format!("%{needle}%");
        let mut st = self.conn.prepare(
            "SELECT * FROM packages WHERE name LIKE ?1 COLLATE NOCASE OR rel_path LIKE ?1 COLLATE NOCASE \
             OR title LIKE ?1 COLLATE NOCASE ORDER BY name",
        )?;
        let rows: Vec<Package> = st.query_map(params![like], package_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_package(
        &self,
        root: &str,
        rel_path: &str,
        name: &str,
        publisher: &str,
        category: &str,
        size: i64,
        mtime: f64,
        header: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<i64> {
        let hs = |k: &str| header.and_then(|h| h.get(k)).and_then(|v| v.as_str()).map(|s| s.to_string());
        let sub_label = |k: &str| {
            header.and_then(|h| h.get(k)).and_then(|v| v.get("label")).and_then(|v| v.as_str()).map(|s| s.to_string())
        };
        let store_id = header.and_then(|h| h.get("id")).map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        let pub_label = sub_label("publisher").filter(|s| !s.is_empty()).unwrap_or_else(|| publisher.to_string());
        let header_json = header.map(|h| serde_json::Value::Object(h.clone()).to_string());
        self.conn.execute(
            r#"INSERT INTO packages(rel_path, name, publisher, category, size, mtime, title, version, unity_version,
                                    pubdate, store_id, category_label, description, header, root)
               VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
               ON CONFLICT(root, rel_path) DO UPDATE SET name=excluded.name, publisher=excluded.publisher,
                 category=excluded.category, size=excluded.size, mtime=excluded.mtime,
                 title=COALESCE(excluded.title, packages.title), version=COALESCE(excluded.version, packages.version),
                 unity_version=COALESCE(excluded.unity_version, packages.unity_version),
                 pubdate=COALESCE(excluded.pubdate, packages.pubdate), store_id=COALESCE(excluded.store_id, packages.store_id),
                 category_label=COALESCE(excluded.category_label, packages.category_label),
                 description=COALESCE(excluded.description, packages.description),
                 header=COALESCE(excluded.header, packages.header)"#,
            params![
                rel_path,
                name,
                pub_label,
                category,
                size,
                mtime,
                hs("title"),
                hs("version"),
                hs("unity_version"),
                hs("pubdate"),
                store_id,
                sub_label("category"),
                hs("description"),
                header_json,
                root
            ],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM packages WHERE root=?1 AND rel_path=?2",
            params![root, rel_path],
            |r| r.get(0),
        )?)
    }

    pub fn mark_package(&self, pid: i64, status: &str, error: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE packages SET status=?1, error=?2 WHERE id=?3", params![status, error, pid])?;
        Ok(())
    }

    pub fn delete_package(&self, pid: i64) -> Result<()> {
        self.clear_package_assets(pid)?;
        self.conn.execute("DELETE FROM packages WHERE id=?1", params![pid])?;
        Ok(())
    }

    pub fn clear_package_assets(&self, pid: i64) -> Result<()> {
        if self.has_fts {
            self.conn.execute(
                "DELETE FROM assets_fts WHERE asset_id IN (SELECT id FROM assets WHERE package_id=?1)",
                params![pid],
            )?;
        }
        self.conn
            .execute("DELETE FROM refs WHERE asset_id IN (SELECT id FROM assets WHERE package_id=?1)", params![pid])?;
        self.conn.execute("DELETE FROM assets WHERE package_id=?1", params![pid])?;
        Ok(())
    }

    /// Replace all assets of a package with freshly scanned entries. Returns (count, total_bytes).
    pub fn replace_package_assets(
        &mut self,
        pid: i64,
        entries: &[Entry],
        previews_indexed: bool,
    ) -> Result<(usize, i64)> {
        let pkg = self.package_by_id(pid)?;
        let (pkg_text, pkg_pub) = match &pkg {
            Some(p) => (format!("{} {}", p.name, p.title.clone().unwrap_or_default()), p.publisher.clone()),
            None => (String::new(), String::new()),
        };
        let has_fts = self.has_fts;
        let tx = self.conn.transaction()?;
        {
            if has_fts {
                tx.execute(
                    "DELETE FROM assets_fts WHERE asset_id IN (SELECT id FROM assets WHERE package_id=?1)",
                    params![pid],
                )?;
            }
            tx.execute("DELETE FROM refs WHERE asset_id IN (SELECT id FROM assets WHERE package_id=?1)", params![pid])?;
            tx.execute("DELETE FROM assets WHERE package_id=?1", params![pid])?;
            let mut ins = tx.prepare_cached(
                "INSERT INTO assets(package_id, guid, path, name, ext, kind, importer, main_class, size,
                       is_folder, has_preview, is_text, scan_truncated, labels)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            )?;
            let mut ins_ref = tx.prepare_cached("INSERT OR IGNORE INTO refs(asset_id, dep_guid) VALUES(?1,?2)")?;
            let mut ins_fts = tx.prepare_cached(
                "INSERT INTO assets_fts(asset_id, name, path, tokens, package, publisher, labels) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            )?;
            let mut count = 0usize;
            let mut total = 0i64;
            for e in entries {
                let (ext, kind) = e.ext_kind();
                let main_class = e.main_class();
                let labels = if e.labels.is_empty() { None } else { Some(serde_json::to_string(&e.labels)?) };
                ins.execute(params![
                    pid,
                    e.guid,
                    e.path,
                    e.name(),
                    ext,
                    kind,
                    e.importer,
                    main_class,
                    e.asset_size as i64,
                    e.is_folder as i64,
                    e.has_preview as i64,
                    e.is_text as i64,
                    e.scan_truncated as i64,
                    labels
                ])?;
                let aid = tx.last_insert_rowid();
                for g in &e.refs {
                    ins_ref.execute(params![aid, g])?;
                }
                if has_fts {
                    ins_fts.execute(params![
                        aid,
                        e.name(),
                        e.path,
                        tokens_for(&e.path, kind, &ext, main_class.as_deref()),
                        pkg_text,
                        pkg_pub,
                        e.labels.join(" ")
                    ])?;
                }
                count += 1;
                total += e.asset_size as i64;
            }
            tx.execute(
                "UPDATE packages SET indexed_at=?1, entry_count=?2, total_bytes=?3, status='ok', error=NULL, previews_indexed=?4 WHERE id=?5",
                params![now(), count as i64, total, previews_indexed as i64, pid],
            )?;
            drop(ins);
            drop(ins_ref);
            drop(ins_fts);
            tx.commit()?;
            Ok((count, total))
        }
    }

    // ----- previews -----------------------------------------------------------------------------

    pub fn put_previews(&mut self, items: &[(String, Vec<u8>)]) -> Result<usize> {
        if items.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut st = tx.prepare_cached("INSERT OR REPLACE INTO pv.previews(guid, png, size) VALUES(?1,?2,?3)")?;
            for (guid, png) in items {
                st.execute(params![guid, png, png.len() as i64])?;
                n += 1;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn get_preview(&self, guid: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row("SELECT png FROM pv.previews WHERE guid=?1", params![guid.to_ascii_lowercase()], |r| r.get(0))
            .optional()?)
    }

    pub fn preview_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM pv.previews", [], |r| r.get(0))?)
    }

    // ----- assets -------------------------------------------------------------------------------

    fn select(
        &self,
        where_: &str,
        params: &[&dyn rusqlite::ToSql],
        order: &str,
        limit: Option<usize>,
        offset: usize,
    ) -> Result<Vec<Asset>> {
        let mut sql = format!("SELECT {ASSET_COLS} FROM assets a JOIN packages p ON p.id=a.package_id WHERE {where_}");
        if !order.is_empty() {
            sql.push_str(" ORDER BY ");
            sql.push_str(order);
        }
        if let Some(l) = limit {
            sql.push_str(&format!(" LIMIT {l}"));
            if offset > 0 {
                sql.push_str(&format!(" OFFSET {offset}"));
            }
        }
        let mut st = self.conn.prepare_cached(&sql)?;
        let rows = st.query_map(params_from_iter(params.iter()), asset_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn asset_by_id(&self, aid: i64) -> Result<Option<Asset>> {
        Ok(self.select("a.id=?1", &[&aid], "", None, 0)?.into_iter().next())
    }

    pub fn assets_by_guid(&self, guid: &str) -> Result<Vec<Asset>> {
        let g = guid.to_ascii_lowercase();
        self.select("a.guid=?1", &[&g], "a.package_id", None, 0)
    }

    pub fn assets_by_path(&self, path: &str, package_id: Option<i64>) -> Result<Vec<Asset>> {
        match package_id {
            None => self.select("a.path=?1 COLLATE NOCASE", &[&path], "a.package_id", None, 0),
            Some(pid) => self.select("a.path=?1 COLLATE NOCASE AND a.package_id=?2", &[&path, &pid], "", None, 0),
        }
    }

    pub fn assets_by_path_suffix(&self, suffix: &str, package_id: Option<i64>, limit: usize) -> Result<Vec<Asset>> {
        let suffix = suffix.trim_start_matches('/');
        let like = format!("%/{suffix}");
        match package_id {
            None => {
                self.select("a.path LIKE ?1 COLLATE NOCASE", &[&like], "length(a.path), a.package_id", Some(limit), 0)
            }
            Some(pid) => self.select(
                "a.path LIKE ?1 COLLATE NOCASE AND a.package_id=?2",
                &[&like, &pid],
                "length(a.path), a.package_id",
                Some(limit),
                0,
            ),
        }
    }

    pub fn assets_by_name(&self, name: &str, package_id: Option<i64>, limit: usize) -> Result<Vec<Asset>> {
        match package_id {
            None => self.select("a.name=?1 COLLATE NOCASE", &[&name], "a.package_id, a.path", Some(limit), 0),
            Some(pid) => self.select(
                "a.name=?1 COLLATE NOCASE AND a.package_id=?2",
                &[&name, &pid],
                "a.package_id, a.path",
                Some(limit),
                0,
            ),
        }
    }

    pub fn folder_assets_for_paths(&self, package_id: i64, dir_paths: &[String]) -> Result<Vec<Asset>> {
        let mut out = Vec::new();
        for chunk in dir_paths.chunks(500) {
            let marks = (0..chunk.len()).map(|i| format!("?{}", i + 2)).collect::<Vec<_>>().join(",");
            let where_ = format!("a.package_id=?1 AND a.is_folder=1 AND a.path IN ({marks})");
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&package_id];
            for p in chunk {
                params.push(p);
            }
            out.extend(self.select(&where_, &params, "", None, 0)?);
        }
        Ok(out)
    }

    pub fn list_package_assets(
        &self,
        package_id: i64,
        prefix: Option<&str>,
        kind: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<Asset>> {
        let mut where_ = String::from("a.package_id=?1");
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(package_id)];
        if let Some(p) = prefix.filter(|p| !p.is_empty()) {
            params.push(Box::new(format!("{}%", p.trim_end_matches('/'))));
            where_.push_str(&format!(" AND a.path LIKE ?{} COLLATE NOCASE", params.len()));
        }
        if let Some(k) = kind.filter(|k| !k.is_empty()) {
            let kinds: Vec<String> = k.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            let mut marks = Vec::new();
            for k in kinds {
                params.push(Box::new(k));
                marks.push(format!("?{}", params.len()));
            }
            where_.push_str(&format!(" AND a.kind IN ({})", marks.join(",")));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        self.select(&where_, &refs, "a.path", limit, 0)
    }

    pub fn refs_of(&self, asset_id: i64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT dep_guid FROM refs WHERE asset_id=?1 ORDER BY dep_guid")?;
        let rows = st.query_map(params![asset_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn referrers(&self, guid: &str, limit: Option<usize>) -> Result<Vec<Asset>> {
        let g = guid.to_ascii_lowercase();
        self.select("a.id IN (SELECT asset_id FROM refs WHERE dep_guid=?1)", &[&g], "p.name, a.path", limit, 0)
    }

    pub fn kind_counts(&self, package_id: Option<i64>) -> Result<Vec<KindCount>> {
        let map = |r: &Row<'_>| {
            Ok(KindCount {
                kind: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                n: r.get(1)?,
                bytes: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            })
        };
        let rows = match package_id {
            None => {
                let mut st = self
                    .conn
                    .prepare("SELECT kind, COUNT(*) n, SUM(size) bytes FROM assets GROUP BY kind ORDER BY n DESC")?;
                let rows: Vec<KindCount> = st.query_map([], map)?.collect::<rusqlite::Result<_>>()?;
                rows
            }
            Some(pid) => {
                let mut st = self.conn.prepare(
                    "SELECT kind, COUNT(*) n, SUM(size) bytes FROM assets WHERE package_id=?1 GROUP BY kind ORDER BY n DESC",
                )?;
                let rows: Vec<KindCount> = st.query_map(params![pid], map)?.collect::<rusqlite::Result<_>>()?;
                rows
            }
        };
        Ok(rows)
    }

    pub fn publishers(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT DISTINCT publisher FROM packages WHERE publisher IS NOT NULL AND publisher != '' ORDER BY publisher COLLATE NOCASE")?;
        let rows: Vec<String> = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn stats(&self) -> Result<Stats> {
        let (packages, assets, refs, bytes): (i64, i64, i64, i64) = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM packages), (SELECT COUNT(*) FROM assets), \
             (SELECT COUNT(*) FROM refs), (SELECT COALESCE(SUM(size),0) FROM assets)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        Ok(Stats {
            packages,
            assets,
            refs,
            bytes,
            previews: self.preview_count().unwrap_or(0),
            fts: self.has_fts,
            db: self.path.to_string_lossy().to_string(),
        })
    }

    // ----- search -------------------------------------------------------------------------------

    /// Turn free text into an FTS5 query: every word (and every camelCase/underscore piece) prefix-matched.
    pub fn fts_query(q: &str) -> String {
        let mut terms = Vec::new();
        for t in q.split_whitespace() {
            let t = t.replace('"', "\"\"");
            for sub in t.split(['_', '-', '.', '/', '\\']) {
                if !sub.is_empty() {
                    terms.push(format!("\"{sub}\"*"));
                }
            }
        }
        terms.join(" ")
    }

    pub fn search(&self, q: &SearchQuery) -> Result<Vec<Asset>> {
        let mut filters: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let push = |params: &mut Vec<Box<dyn rusqlite::ToSql>>, v: String| -> String {
            params.push(Box::new(v));
            format!("?{}", params.len())
        };
        if let Some(kind) = q.kind.as_deref().filter(|k| !k.trim().is_empty()) {
            let marks: Vec<String> = kind
                .split(',')
                .map(|k| k.trim())
                .filter(|k| !k.is_empty())
                .map(|k| push(&mut params, k.to_string()))
                .collect();
            filters.push(format!("a.kind IN ({})", marks.join(",")));
        }
        if let Some(ext) = q.ext.as_deref().filter(|e| !e.trim().is_empty()) {
            let m = push(&mut params, ext.trim().trim_start_matches('.').to_string());
            filters.push(format!("a.ext = {m} COLLATE NOCASE"));
        }
        if let Some(p) = q.package.as_deref().filter(|p| !p.trim().is_empty()) {
            if p.bytes().all(|b| b.is_ascii_digit()) {
                let m = push(&mut params, p.to_string());
                filters.push(format!("p.id = {m}"));
            } else {
                let m = push(&mut params, format!("%{}%", p.trim()));
                filters.push(format!("(p.name LIKE {m} COLLATE NOCASE OR p.rel_path LIKE {m} COLLATE NOCASE OR p.title LIKE {m} COLLATE NOCASE)"));
            }
        }
        if let Some(p) = q.publisher.as_deref().filter(|p| !p.trim().is_empty()) {
            let m = push(&mut params, format!("%{}%", p.trim()));
            filters.push(format!("p.publisher LIKE {m} COLLATE NOCASE"));
        }
        if !q.include_folders {
            filters.push("a.is_folder=0".into());
        }
        let extra = if filters.is_empty() { String::new() } else { format!(" AND {}", filters.join(" AND ")) };
        let query = q.query.trim();
        let limit = q.limit.max(1);
        if query.is_empty() {
            let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
            return self.select(&format!("1=1{extra}"), &refs, "p.name, a.path", Some(limit), q.offset);
        }
        if self.has_fts {
            let fq = Self::fts_query(query);
            if !fq.is_empty() {
                let fq_mark = push(&mut params, fq);
                let sql = format!(
                    "SELECT {ASSET_COLS}, bm25(assets_fts, 0, 10.0, 3.0, 5.0, 1.0, 1.0, 2.0) AS rank \
                     FROM assets_fts JOIN assets a ON a.id = assets_fts.asset_id JOIN packages p ON p.id=a.package_id \
                     WHERE assets_fts MATCH {fq_mark}{extra} ORDER BY rank, length(a.path) LIMIT {limit} OFFSET {}",
                    q.offset
                );
                let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
                if let Ok(mut st) = self.conn.prepare_cached(&sql) {
                    if let Ok(rows) = st.query_map(params_from_iter(refs.iter()), asset_from_row) {
                        let rows: Vec<Asset> = rows.filter_map(|r| r.ok()).collect();
                        if !rows.is_empty() {
                            return Ok(rows);
                        }
                    }
                }
                params.pop();
            }
        }
        // Fallback: every term must appear as a substring of the path (case-insensitive).
        let mut where_ = Vec::new();
        for t in query.split_whitespace() {
            let m = push(&mut params, format!("%{t}%"));
            where_.push(format!("a.path LIKE {m} COLLATE NOCASE"));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        self.select(&format!("{}{extra}", where_.join(" AND ")), &refs, "length(a.path), a.path", Some(limit), q.offset)
    }
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        assert_eq!(camel_tokens("SM_Env_Tree01.fbx"), "SM Env Tree 01 fbx");
        assert_eq!(camel_tokens("Assets/KayKit/chestGold"), "Assets Kay Kit chest Gold");
    }

    #[test]
    fn fts() {
        assert_eq!(Database::fts_query("sm_env tree"), "\"sm\"* \"env\"* \"tree\"*");
    }
}
