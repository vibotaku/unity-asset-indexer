"""SQLite storage for the asset index (with FTS5 full-text search when available)."""
from __future__ import annotations

import json
import re
import sqlite3
import time
from pathlib import Path
from typing import Iterable, Sequence

SCHEMA_VERSION = 1

SCHEMA = """
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS packages (
    id INTEGER PRIMARY KEY,
    rel_path TEXT UNIQUE NOT NULL,
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
    header TEXT
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
"""

FTS_SCHEMA = """
CREATE VIRTUAL TABLE IF NOT EXISTS assets_fts USING fts5(
    asset_id UNINDEXED, name, path, tokens, package, publisher, labels,
    tokenize = 'unicode61'
);
"""


def _camel_tokens(s: str) -> str:
    s = re.sub(r"([a-z0-9])([A-Z])", r"\1 \2", s)
    s = re.sub(r"([A-Za-z])(\d)", r"\1 \2", s)
    s = re.sub(r"(\d)([A-Za-z])", r"\1 \2", s)
    return re.sub(r"[_\-./\\]+", " ", s)


def tokens_for(path: str, kind: str, ext: str, main_class: str | None) -> str:
    parts = [_camel_tokens(path), kind or "", ext or "", main_class or ""]
    return " ".join(p for p in parts if p)


class Database:
    def __init__(self, path: Path | str):
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.conn = sqlite3.connect(str(self.path), timeout=60)
        self.conn.row_factory = sqlite3.Row
        self.conn.execute("PRAGMA journal_mode=WAL")
        self.conn.execute("PRAGMA synchronous=NORMAL")
        self.conn.execute("PRAGMA foreign_keys=ON")
        self.conn.executescript(SCHEMA)
        self.has_fts = self._init_fts()
        self.conn.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', ?)", (str(SCHEMA_VERSION),))
        self.conn.commit()

    def _init_fts(self) -> bool:
        try:
            self.conn.executescript(FTS_SCHEMA)
            return True
        except sqlite3.OperationalError:
            return False

    def close(self) -> None:
        self.conn.close()

    # ----- packages -----------------------------------------------------------------------------
    def package_by_rel_path(self, rel_path: str) -> sqlite3.Row | None:
        return self.conn.execute("SELECT * FROM packages WHERE rel_path=?", (rel_path,)).fetchone()

    def package_by_id(self, pid: int) -> sqlite3.Row | None:
        return self.conn.execute("SELECT * FROM packages WHERE id=?", (pid,)).fetchone()

    def packages(self) -> list[sqlite3.Row]:
        return self.conn.execute("SELECT * FROM packages ORDER BY publisher, name").fetchall()

    def find_packages(self, needle: str) -> list[sqlite3.Row]:
        """Match by exact id, exact name, or case-insensitive substring of name/rel_path."""
        if needle.isdigit():
            row = self.package_by_id(int(needle))
            return [row] if row else []
        rows = self.conn.execute("SELECT * FROM packages WHERE name = ? COLLATE NOCASE", (needle,)).fetchall()
        if rows:
            return rows
        like = f"%{needle}%"
        return self.conn.execute(
            "SELECT * FROM packages WHERE name LIKE ? COLLATE NOCASE OR rel_path LIKE ? COLLATE NOCASE ORDER BY name",
            (like, like),
        ).fetchall()

    def upsert_package(self, rel_path: str, name: str, publisher: str, category: str, size: int, mtime: float,
                       header: dict | None = None) -> int:
        h = header or {}
        pub = h.get("publisher") or {}
        cat = h.get("category") or {}
        self.conn.execute(
            """INSERT INTO packages(rel_path, name, publisher, category, size, mtime, title, version, unity_version,
                                    pubdate, store_id, category_label, description, header)
               VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)
               ON CONFLICT(rel_path) DO UPDATE SET name=excluded.name, publisher=excluded.publisher,
                 category=excluded.category, size=excluded.size, mtime=excluded.mtime,
                 title=COALESCE(excluded.title, packages.title), version=COALESCE(excluded.version, packages.version),
                 unity_version=COALESCE(excluded.unity_version, packages.unity_version),
                 pubdate=COALESCE(excluded.pubdate, packages.pubdate), store_id=COALESCE(excluded.store_id, packages.store_id),
                 category_label=COALESCE(excluded.category_label, packages.category_label),
                 description=COALESCE(excluded.description, packages.description),
                 header=COALESCE(excluded.header, packages.header)""",
            (rel_path, name, (pub.get("label") if isinstance(pub, dict) else None) or publisher, category, size, mtime,
             h.get("title"), h.get("version"), h.get("unity_version"), h.get("pubdate"), h.get("id"),
             cat.get("label") if isinstance(cat, dict) else None, h.get("description"),
             json.dumps(h) if h else None),
        )
        return self.conn.execute("SELECT id FROM packages WHERE rel_path=?", (rel_path,)).fetchone()[0]

    def mark_package(self, pid: int, status: str, error: str | None = None) -> None:
        self.conn.execute("UPDATE packages SET status=?, error=? WHERE id=?", (status, error, pid))
        self.conn.commit()

    def delete_package(self, pid: int) -> None:
        self.clear_package_assets(pid)
        self.conn.execute("DELETE FROM packages WHERE id=?", (pid,))
        self.conn.commit()

    def clear_package_assets(self, pid: int) -> None:
        if self.has_fts:
            self.conn.execute(
                "DELETE FROM assets_fts WHERE asset_id IN (SELECT id FROM assets WHERE package_id=?)", (pid,)
            )
        self.conn.execute("DELETE FROM refs WHERE asset_id IN (SELECT id FROM assets WHERE package_id=?)", (pid,))
        self.conn.execute("DELETE FROM assets WHERE package_id=?", (pid,))

    def replace_package_assets(self, pid: int, rows: Iterable[dict]) -> tuple[int, int]:
        """Replace all assets of a package. rows are Entry.to_row() dicts. Returns (count, total_bytes)."""
        pkg = self.package_by_id(pid)
        self.clear_package_assets(pid)
        count = 0
        total = 0
        cur = self.conn.cursor()
        for r in rows:
            cur.execute(
                """INSERT INTO assets(package_id, guid, path, name, ext, kind, importer, main_class, size,
                       is_folder, has_preview, is_text, scan_truncated, labels)
                   VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
                (
                    pid, r["guid"], r["path"], r["name"], r["ext"], r["kind"], r["importer"], r["main_class"],
                    r["size"], int(r["is_folder"]), int(r["has_preview"]), int(r["is_text"]),
                    int(r["scan_truncated"]), json.dumps(r["labels"]) if r["labels"] else None,
                ),
            )
            aid = cur.lastrowid
            if r["refs"]:
                cur.executemany("INSERT OR IGNORE INTO refs(asset_id, dep_guid) VALUES(?,?)", [(aid, g) for g in r["refs"]])
            if self.has_fts:
                cur.execute(
                    "INSERT INTO assets_fts(asset_id, name, path, tokens, package, publisher, labels) VALUES(?,?,?,?,?,?,?)",
                    (
                        aid, r["name"], r["path"], tokens_for(r["path"], r["kind"], r["ext"], r["main_class"]),
                        (f"{pkg['name']} {pkg['title'] or ''}" if pkg else ""), pkg["publisher"] if pkg else "",
                        " ".join(r["labels"] or []),
                    ),
                )
            count += 1
            total += r["size"] or 0
        self.conn.execute(
            "UPDATE packages SET indexed_at=?, entry_count=?, total_bytes=?, status='ok', error=NULL WHERE id=?",
            (time.time(), count, total, pid),
        )
        self.conn.commit()
        return count, total

    # ----- assets ------------------------------------------------------------------------------------
    ASSET_COLS = (
        "a.id, a.package_id, a.guid, a.path, a.name, a.ext, a.kind, a.importer, a.main_class, a.size, "
        "a.is_folder, a.has_preview, a.is_text, a.scan_truncated, a.labels, "
        "p.name AS package, p.publisher AS publisher, p.rel_path AS package_rel_path"
    )

    def _select(self, where: str, params: Sequence, order: str = "", limit: int | None = None) -> list[sqlite3.Row]:
        sql = f"SELECT {self.ASSET_COLS} FROM assets a JOIN packages p ON p.id=a.package_id WHERE {where}"
        if order:
            sql += f" ORDER BY {order}"
        if limit:
            sql += f" LIMIT {int(limit)}"
        return self.conn.execute(sql, params).fetchall()

    def asset_by_id(self, aid: int) -> sqlite3.Row | None:
        rows = self._select("a.id=?", (aid,))
        return rows[0] if rows else None

    def assets_by_guid(self, guid: str) -> list[sqlite3.Row]:
        return self._select("a.guid=?", (guid.lower(),), order="a.package_id")

    def assets_by_path(self, path: str, package_id: int | None = None) -> list[sqlite3.Row]:
        if package_id is None:
            return self._select("a.path=? COLLATE NOCASE", (path,), order="a.package_id")
        return self._select("a.path=? COLLATE NOCASE AND a.package_id=?", (path, package_id))

    def assets_by_path_suffix(self, suffix: str, package_id: int | None = None, limit: int = 50) -> list[sqlite3.Row]:
        suffix = suffix.lstrip("/")
        params: list = [f"%/{suffix}"]
        where = "a.path LIKE ? COLLATE NOCASE"
        if package_id is not None:
            where += " AND a.package_id=?"
            params.append(package_id)
        return self._select(where, params, order="length(a.path), a.package_id", limit=limit)

    def assets_by_name(self, name: str, package_id: int | None = None, limit: int = 50) -> list[sqlite3.Row]:
        params: list = [name]
        where = "a.name=? COLLATE NOCASE"
        if package_id is not None:
            where += " AND a.package_id=?"
            params.append(package_id)
        return self._select(where, params, order="a.package_id, a.path", limit=limit)

    def folder_assets_for_paths(self, package_id: int, dir_paths: Iterable[str]) -> list[sqlite3.Row]:
        dirs = [d for d in set(dir_paths) if d]
        if not dirs:
            return []
        out: list[sqlite3.Row] = []
        for i in range(0, len(dirs), 500):
            chunk = dirs[i:i + 500]
            q = ",".join("?" * len(chunk))
            out.extend(self._select(f"a.package_id=? AND a.is_folder=1 AND a.path IN ({q})", [package_id, *chunk]))
        return out

    def list_package_assets(self, package_id: int, prefix: str | None = None, kind: str | None = None,
                            limit: int | None = None) -> list[sqlite3.Row]:
        where = "a.package_id=?"
        params: list = [package_id]
        if prefix:
            where += " AND a.path LIKE ? COLLATE NOCASE"
            params.append(prefix.rstrip("/") + "%")
        if kind:
            where += " AND a.kind=?"
            params.append(kind)
        return self._select(where, params, order="a.path", limit=limit)

    def refs_of(self, asset_id: int) -> list[str]:
        return [r[0] for r in self.conn.execute("SELECT dep_guid FROM refs WHERE asset_id=?", (asset_id,))]

    def referrers(self, guid: str, limit: int | None = None) -> list[sqlite3.Row]:
        return self._select("a.id IN (SELECT asset_id FROM refs WHERE dep_guid=?)", (guid.lower(),), order="p.name, a.path", limit=limit)

    def kind_counts(self, package_id: int | None = None) -> list[sqlite3.Row]:
        if package_id is None:
            return self.conn.execute("SELECT kind, COUNT(*) n, SUM(size) bytes FROM assets GROUP BY kind ORDER BY n DESC").fetchall()
        return self.conn.execute(
            "SELECT kind, COUNT(*) n, SUM(size) bytes FROM assets WHERE package_id=? GROUP BY kind ORDER BY n DESC", (package_id,)
        ).fetchall()

    def stats(self) -> dict:
        r = self.conn.execute(
            "SELECT (SELECT COUNT(*) FROM packages) pk, (SELECT COUNT(*) FROM assets) a, "
            "(SELECT COUNT(*) FROM refs) r, (SELECT COALESCE(SUM(size),0) FROM assets) b"
        ).fetchone()
        return {"packages": r[0], "assets": r[1], "refs": r[2], "bytes": r[3], "fts": self.has_fts, "db": str(self.path)}

    # ----- search ------------------------------------------------------------------------------------
    @staticmethod
    def _fts_query(q: str) -> str:
        terms = []
        for t in re.split(r"\s+", q.strip()):
            if not t:
                continue
            t = t.replace('"', '""')
            # Prefix-match every term; split camelCase/underscores the same way tokens are built.
            for sub in re.split(r"[_\-./\\]+", t):
                if sub:
                    terms.append(f'"{sub}"*')
        return " ".join(terms)

    def search(self, query: str, kind: str | None = None, package: str | None = None, publisher: str | None = None,
               ext: str | None = None, include_folders: bool = False, limit: int = 50) -> list[sqlite3.Row]:
        filters = []
        params: list = []
        if kind:
            kinds = [k.strip() for k in kind.split(",") if k.strip()]
            filters.append(f"a.kind IN ({','.join('?' * len(kinds))})")
            params.extend(kinds)
        if ext:
            filters.append("a.ext = ? COLLATE NOCASE")
            params.append(ext.lstrip("."))
        if package:
            filters.append("(p.name LIKE ? COLLATE NOCASE OR p.rel_path LIKE ? COLLATE NOCASE)")
            params.extend([f"%{package}%", f"%{package}%"])
        if publisher:
            filters.append("p.publisher LIKE ? COLLATE NOCASE")
            params.append(f"%{publisher}%")
        if not include_folders:
            filters.append("a.is_folder=0")
        extra = (" AND " + " AND ".join(filters)) if filters else ""
        query = query.strip()
        if not query:
            return self._select("1=1" + extra, params, order="a.path", limit=limit)
        if self.has_fts:
            fq = self._fts_query(query)
            if fq:
                sql = (
                    f"SELECT {self.ASSET_COLS}, bm25(assets_fts, 0, 10.0, 3.0, 5.0, 1.0, 1.0, 2.0) AS rank "
                    f"FROM assets_fts JOIN assets a ON a.id = assets_fts.asset_id JOIN packages p ON p.id=a.package_id "
                    f"WHERE assets_fts MATCH ? {extra} ORDER BY rank, length(a.path) LIMIT ?"
                )
                try:
                    rows = self.conn.execute(sql, [fq, *params, limit]).fetchall()
                    if rows:
                        return rows
                except sqlite3.OperationalError:
                    pass
        # Fallback: every term must appear as a substring of the path (case-insensitive).
        terms = [t for t in re.split(r"\s+", query) if t]
        where = " AND ".join("a.path LIKE ? COLLATE NOCASE" for _ in terms)
        return self._select(where + extra, [f"%{t}%" for t in terms] + params, order="length(a.path), a.path", limit=limit)


def row_to_dict(row: sqlite3.Row | None) -> dict | None:
    if row is None:
        return None
    d = dict(row)
    if "labels" in d and isinstance(d["labels"], str):
        try:
            d["labels"] = json.loads(d["labels"])
        except json.JSONDecodeError:
            d["labels"] = []
    elif "labels" in d and d["labels"] is None:
        d["labels"] = []
    for k in ("is_folder", "has_preview", "is_text", "scan_truncated"):
        if k in d:
            d[k] = bool(d[k])
    d.pop("rank", None)
    return d
