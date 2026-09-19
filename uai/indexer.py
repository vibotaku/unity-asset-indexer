"""Walk the library, stream each .unitypackage once, and store results in SQLite."""
from __future__ import annotations

import multiprocessing
import os
import sys
import time
import traceback
from concurrent.futures import ProcessPoolExecutor, wait, FIRST_COMPLETED
from pathlib import Path
from typing import Callable, Iterator

from .config import Config
from .db import Database
from . import unitypackage
from .unitypackage import iter_package, read_package_header


def _init_worker(counter) -> None:
    unitypackage.PROGRESS_COUNTER = counter


def _fmt_bytes(n: float) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024 or unit == "GB":
            return f"{n:.0f}{unit}" if unit in ("B", "KB") else f"{n:.2f}{unit}"
        n /= 1024
    return f"{n:.2f}GB"


def _fmt_eta(seconds: float) -> str:
    seconds = max(0, int(seconds))
    return f"{seconds // 3600}h{(seconds % 3600) // 60:02d}m" if seconds >= 3600 else f"{seconds // 60}m{seconds % 60:02d}s"


def discover_packages(library: Path) -> list[Path]:
    out: list[Path] = []
    for root, dirs, files in os.walk(library):
        dirs[:] = [d for d in dirs if not d.startswith(".")]
        for f in files:
            if f.lower().endswith(".unitypackage") and not f.startswith("."):
                out.append(Path(root) / f)
    return sorted(out)


def split_rel_path(rel: str) -> tuple[str, str, str]:
    """`Publisher/Category/Name.unitypackage` -> (name, publisher, category). Tolerates flatter layouts."""
    parts = rel.split("/")
    name = parts[-1]
    if name.lower().endswith(".unitypackage"):
        name = name[: -len(".unitypackage")]
    publisher = parts[0] if len(parts) >= 2 else ""
    category = "/".join(parts[1:-1]) if len(parts) >= 3 else ""
    return name, publisher, category


def _scan_worker(path: str) -> dict:
    t0 = time.time()
    try:
        rows = [e.to_row() for e in iter_package(path)]
        return {"path": path, "rows": rows, "seconds": time.time() - t0, "error": None}
    except Exception as e:  # noqa: BLE001 - report per package, keep going
        return {"path": path, "rows": [], "seconds": time.time() - t0, "error": f"{e}\n{traceback.format_exc()}"}


def index_library(cfg: Config, db: Database, *, workers: int = 4, force: bool = False,
                  only: str | None = None, log: Callable[[str], None] = print,
                  progress: Callable[[str], None] | None = None) -> dict:
    """Index the library. `log` gets one line per finished package; `progress` (optional) gets a
    periodically refreshed status line (bytes read, throughput, ETA)."""
    lib = cfg.library
    if not lib.exists():
        raise FileNotFoundError(f"Library not found: {lib} (is the share mounted? set UAI_LIBRARY or `uai config --library`)")
    paths = discover_packages(lib)
    if only:
        paths = [p for p in paths if only.lower() in str(p.relative_to(lib)).lower()]
    seen_rel: set[str] = set()
    todo: list[tuple[int, Path]] = []
    skipped = 0
    for p in paths:
        rel = str(p.relative_to(lib))
        seen_rel.add(rel)
        st = p.stat()
        existing = db.package_by_rel_path(rel)
        name, publisher, category = split_rel_path(rel)
        header = read_package_header(str(p)) if (force or not existing or existing["size"] != st.st_size
                                                  or abs((existing["mtime"] or 0) - st.st_mtime) >= 1
                                                  or not existing["header"]) else None
        pid = db.upsert_package(rel, name, publisher, category, st.st_size, st.st_mtime, header)
        if (existing and not force and existing["status"] == "ok" and existing["indexed_at"]
                and existing["size"] == st.st_size and abs((existing["mtime"] or 0) - st.st_mtime) < 1):
            skipped += 1
            continue
        todo.append((pid, p))
    db.conn.commit()

    removed = 0
    if not only:
        for row in db.packages():
            if row["rel_path"] not in seen_rel:
                log(f"- removed from library: {row['rel_path']}")
                db.delete_package(row["id"])
                removed += 1

    log(f"{len(paths)} packages found, {skipped} up to date, {len(todo)} to index, {removed} removed")
    # Largest first so the long tail doesn't end up serialized on one worker at the end.
    todo.sort(key=lambda t: t[1].stat().st_size, reverse=True)
    total_entries = 0
    errors = 0
    t_start = time.time()
    if todo:
        total_bytes = sum(p.stat().st_size for _, p in todo)
        counter = multiprocessing.Value("q", 0)
        with ProcessPoolExecutor(max_workers=max(1, workers), initializer=_init_worker, initargs=(counter,)) as ex:
            futs = {ex.submit(_scan_worker, str(p)): (pid, p) for pid, p in todo}
            pending = set(futs)
            done = 0
            last_report = 0.0
            while pending:
                finished, pending = wait(pending, timeout=2.0, return_when=FIRST_COMPLETED)
                for fut in finished:
                    pid, p = futs[fut]
                    res = fut.result()
                    done += 1
                    rel = str(p.relative_to(lib))
                    if res["error"]:
                        errors += 1
                        db.mark_package(pid, "error", res["error"])
                        log(f"[{done}/{len(todo)}] ERROR {rel}: {res['error'].splitlines()[0]}")
                        continue
                    n, total = db.replace_package_assets(pid, res["rows"])
                    total_entries += n
                    log(f"[{done}/{len(todo)}] {rel}  {n} assets, {total / 1e6:.0f} MB uncompressed, {res['seconds']:.0f}s")
                now = time.time()
                if progress and (now - last_report >= 5.0 or not pending):
                    last_report = now
                    read = counter.value
                    elapsed = now - t_start
                    rate = read / elapsed if elapsed > 0 else 0
                    eta = (total_bytes - read) / rate if rate > 0 else 0
                    progress(f"{done}/{len(todo)} packages, {_fmt_bytes(read)}/{_fmt_bytes(total_bytes)} read, "
                             f"{_fmt_bytes(rate)}/s, ETA {_fmt_eta(eta)}")
    return {
        "found": len(paths), "indexed": len(todo) - errors, "skipped": skipped, "removed": removed,
        "errors": errors, "entries": total_entries, "seconds": time.time() - t_start,
    }
