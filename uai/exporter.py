"""Export assets (plus dependencies) into a Unity project, a plain folder, or a slim .unitypackage."""
from __future__ import annotations

import os
import re
import shutil
import tarfile
import time
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath
from typing import BinaryIO, Iterator

from .config import Config
from .db import Database, row_to_dict
from .deps import Closure, resolve_closure

META_GUID_RE = re.compile(r"^guid:\s*([0-9a-fA-F]{32})", re.M)


@dataclass
class Plan:
    closure: Closure
    assets: list[dict]                   # everything to write (roots + deps + folder metas)
    folders: list[dict]                  # subset: folder assets (meta only)
    by_package: dict[int, list[dict]]    # package_id -> assets
    warnings: list[str] = field(default_factory=list)

    @property
    def total_bytes(self) -> int:
        return sum(a["size"] or 0 for a in self.assets)


def build_plan(db: Database, roots: list[dict], *, include_deps: bool = True, include_scripts: bool = True,
               include_folders: bool = True, max_depth: int | None = None) -> Plan:
    closure = resolve_closure(db, roots, include_scripts=include_scripts, max_depth=None if include_deps else 0)
    assets = [n.asset for n in closure.nodes.values()]
    folders: list[dict] = []
    if include_folders:
        by_pkg_dirs: dict[int, set[str]] = {}
        for a in assets:
            parts = PurePosixPath(a["path"]).parts
            for i in range(1, len(parts)):
                by_pkg_dirs.setdefault(a["package_id"], set()).add("/".join(parts[:i]))
        have = {(a["package_id"], a["guid"]) for a in assets}
        for pid, dirs in by_pkg_dirs.items():
            for row in db.folder_assets_for_paths(pid, dirs):
                d = row_to_dict(row)
                if (pid, d["guid"]) not in have:
                    folders.append(d)
                    have.add((pid, d["guid"]))
    all_assets = assets + folders
    by_package: dict[int, list[dict]] = {}
    for a in all_assets:
        by_package.setdefault(a["package_id"], []).append(a)
    warnings: list[str] = []
    scripts = closure.scripts()
    if scripts:
        warnings.append(
            f"{len(scripts)} script/plugin file(s) are in the dependency set. Partial script imports may not compile "
            f"if they depend on other scripts in the package (`--no-scripts` to leave them out)."
        )
    if closure.unresolved:
        warnings.append(f"{len(closure.unresolved)} referenced guid(s) are not in the library (see unresolved).")
    amb = [n for n in closure.nodes.values() if n.ambiguous_in]
    if amb:
        warnings.append(f"{len(amb)} guid(s) exist in more than one package; the referrer's package was preferred.")
    return Plan(closure=closure, assets=all_assets, folders=folders, by_package=by_package, warnings=warnings)


# ----- reading members from a package (cache or stream) -------------------------------------------

def package_abs_path(cfg: Config, pkg_row) -> Path:
    return cfg.library / pkg_row["rel_path"]


def cache_path_for(cfg: Config, pkg_row) -> Path:
    return cfg.cache_dir / str(pkg_row["id"])


def iter_package_members(cfg: Config, pkg_row, guids: set[str]) -> Iterator[tuple[str, str, int, float, BinaryIO]]:
    """Yield (guid, leaf, size, mtime, fileobj). Reads from the local cache when present, else streams the share."""
    cache = cache_path_for(cfg, pkg_row)
    if (cache / ".complete").exists():
        for g in sorted(guids):
            d = cache / g
            if not d.is_dir():
                continue
            for leaf in ("asset", "asset.meta", "pathname", "preview.png"):
                f = d / leaf
                if f.is_file():
                    st = f.stat()
                    with f.open("rb") as fh:
                        yield g, leaf, st.st_size, st.st_mtime, fh
        return
    from .unitypackage import iter_members
    for g, leaf, ti, fh in iter_members(str(package_abs_path(cfg, pkg_row)), guids):
        yield g, leaf, ti.size, ti.mtime, fh


# ----- cache ---------------------------------------------------------------------------------------

def cache_add(cfg: Config, pkg_row, log=print) -> Path:
    dest = cache_path_for(cfg, pkg_row)
    if (dest / ".complete").exists():
        return dest
    tmp = dest.with_suffix(".partial")
    if tmp.exists():
        shutil.rmtree(tmp)
    tmp.mkdir(parents=True)
    src = package_abs_path(cfg, pkg_row)
    log(f"caching {pkg_row['name']} ({src.stat().st_size / 1e6:.0f} MB compressed) -> {dest}")
    t0 = time.time()
    from .unitypackage import open_stream
    with open_stream(str(src)) as tar:
        for m in tar:
            name = m.name[2:] if m.name.startswith("./") else m.name
            parts = [p for p in name.split("/") if p]
            if not parts or len(parts[0]) != 32 or not m.isfile() or len(parts) < 2:
                continue
            out = tmp / parts[0].lower() / parts[1]
            out.parent.mkdir(exist_ok=True)
            with out.open("wb") as fh:
                shutil.copyfileobj(tar.extractfile(m), fh, 1024 * 1024)
    (tmp / ".complete").write_text(str(time.time()))
    if dest.exists():
        shutil.rmtree(dest)
    tmp.rename(dest)
    log(f"cached in {time.time() - t0:.0f}s")
    return dest


def cache_remove(cfg: Config, pkg_row) -> bool:
    dest = cache_path_for(cfg, pkg_row)
    if dest.exists():
        shutil.rmtree(dest)
        return True
    return False


def cache_list(cfg: Config, db: Database) -> list[dict]:
    out = []
    if not cfg.cache_dir.exists():
        return out
    for d in sorted(cfg.cache_dir.iterdir()):
        if not d.is_dir() or not d.name.isdigit():
            continue
        pkg = db.package_by_id(int(d.name))
        size = sum(f.stat().st_size for f in d.rglob("*") if f.is_file())
        out.append({"package_id": int(d.name), "package": pkg["name"] if pkg else "?", "bytes": size,
                    "complete": (d / ".complete").exists(), "path": str(d)})
    return out


# ----- project conflict scan -------------------------------------------------------------------------

def scan_project_guids(project_dir: Path) -> dict[str, str]:
    """guid -> relative path (without .meta) for every .meta under Assets/."""
    out: dict[str, str] = {}
    assets = project_dir / "Assets"
    if not assets.is_dir():
        return out
    for root, dirs, files in os.walk(assets):
        dirs[:] = [d for d in dirs if not d.startswith(".")]
        for f in files:
            if not f.endswith(".meta"):
                continue
            p = Path(root) / f
            try:
                with p.open("rb") as fh:
                    head = fh.read(512).decode("utf-8", "replace")
            except OSError:
                continue
            m = META_GUID_RE.search(head)
            if m:
                out[m.group(1).lower()] = str(p.relative_to(project_dir))[: -len(".meta")].replace(os.sep, "/")
    return out


# ----- export ----------------------------------------------------------------------------------------

@dataclass
class ExportResult:
    written: list[dict] = field(default_factory=list)      # {path, guid, bytes}
    skipped_existing: list[dict] = field(default_factory=list)
    conflicts: list[dict] = field(default_factory=list)     # same guid at a different path in the project
    missing_in_package: list[dict] = field(default_factory=list)
    bytes_written: int = 0
    seconds: float = 0.0
    dry_run: bool = False
    output: str = ""

    def to_dict(self) -> dict:
        return {
            "output": self.output, "dry_run": self.dry_run, "bytes_written": self.bytes_written,
            "seconds": round(self.seconds, 1), "written": self.written, "skipped_existing": self.skipped_existing,
            "conflicts": self.conflicts, "missing_in_package": self.missing_in_package,
        }


def export_to_dir(cfg: Config, db: Database, plan: Plan, dest_root: Path, *, force: bool = False,
                  dry_run: bool = False, project_guids: dict[str, str] | None = None, log=lambda s: None) -> ExportResult:
    """Write `<dest_root>/<Assets/...>` and `.meta` for every planned asset."""
    t0 = time.time()
    res = ExportResult(dry_run=dry_run, output=str(dest_root))
    project_guids = project_guids or {}
    for pid, assets in plan.by_package.items():
        pkg = db.package_by_id(pid)
        todo: dict[str, dict] = {}
        for a in assets:
            target = dest_root / a["path"]
            if a["guid"] in project_guids and project_guids[a["guid"]].lower() != a["path"].lower():
                res.conflicts.append({"guid": a["guid"], "path": a["path"], "existing_path": project_guids[a["guid"]]})
                continue
            exists = target.exists() if not a["is_folder"] else (target.with_name(target.name + ".meta")).exists()
            if exists and not force:
                res.skipped_existing.append({"guid": a["guid"], "path": a["path"]})
                continue
            todo[a["guid"]] = a
        if not todo:
            continue
        log(f"{'would extract' if dry_run else 'extracting'} {len(todo)} from {pkg['name']}")
        if dry_run:
            for a in todo.values():
                res.written.append({"guid": a["guid"], "path": a["path"], "bytes": a["size"] or 0, "meta": True})
                res.bytes_written += a["size"] or 0
            continue
        seen: dict[str, set[str]] = {g: set() for g in todo}
        for guid, leaf, size, mtime, fh in iter_package_members(cfg, pkg, set(todo)):
            a = todo[guid]
            target = dest_root / a["path"]
            if leaf == "asset":
                target.parent.mkdir(parents=True, exist_ok=True)
                with target.open("wb") as out:
                    shutil.copyfileobj(fh, out, 1024 * 1024)
                os.utime(target, (mtime, mtime))
                res.bytes_written += size
                seen[guid].add("asset")
            elif leaf == "asset.meta":
                meta_path = target.with_name(target.name + ".meta")
                meta_path.parent.mkdir(parents=True, exist_ok=True)
                if a["is_folder"]:
                    target.mkdir(parents=True, exist_ok=True)
                with meta_path.open("wb") as out:
                    shutil.copyfileobj(fh, out)
                seen[guid].add("meta")
        for guid, a in todo.items():
            got = seen[guid]
            if "meta" in got and ("asset" in got or a["is_folder"]):
                res.written.append({"guid": guid, "path": a["path"], "bytes": a["size"] or 0, "meta": True})
            else:
                res.missing_in_package.append({"guid": guid, "path": a["path"], "got": sorted(got)})
    res.seconds = time.time() - t0
    return res


def export_to_unitypackage(cfg: Config, db: Database, plan: Plan, out_file: Path, *, dry_run: bool = False,
                           log=lambda s: None) -> ExportResult:
    """Repack the selected guid directories into a new (much smaller) .unitypackage."""
    t0 = time.time()
    res = ExportResult(dry_run=dry_run, output=str(out_file))
    if dry_run:
        for a in plan.assets:
            res.written.append({"guid": a["guid"], "path": a["path"], "bytes": a["size"] or 0})
            res.bytes_written += a["size"] or 0
        res.seconds = time.time() - t0
        return res
    out_file.parent.mkdir(parents=True, exist_ok=True)
    tmp = out_file.with_suffix(out_file.suffix + ".partial")
    with tarfile.open(str(tmp), mode="w:gz", compresslevel=6) as tar:
        for pid, assets in plan.by_package.items():
            pkg = db.package_by_id(pid)
            todo = {a["guid"]: a for a in assets}
            log(f"packing {len(todo)} from {pkg['name']}")
            seen: dict[str, set[str]] = {g: set() for g in todo}
            for guid, leaf, size, mtime, fh in iter_package_members(cfg, pkg, set(todo)):
                ti = tarfile.TarInfo(name=f"{guid}/{leaf}")
                ti.size = size
                ti.mtime = int(mtime)
                ti.mode = 0o644
                tar.addfile(ti, fh)
                seen[guid].add(leaf)
                if leaf == "asset":
                    res.bytes_written += size
            for guid, a in todo.items():
                if "asset.meta" in seen[guid]:
                    res.written.append({"guid": guid, "path": a["path"], "bytes": a["size"] or 0})
                else:
                    res.missing_in_package.append({"guid": guid, "path": a["path"], "got": sorted(seen[guid])})
    tmp.replace(out_file)
    res.seconds = time.time() - t0
    return res


def extract_preview(cfg: Config, db: Database, asset: dict, out_file: Path) -> bool:
    pkg = db.package_by_id(asset["package_id"])
    for guid, leaf, size, mtime, fh in iter_package_members(cfg, pkg, {asset["guid"]}):
        if leaf == "preview.png":
            out_file.parent.mkdir(parents=True, exist_ok=True)
            with out_file.open("wb") as out:
                shutil.copyfileobj(fh, out)
            return True
    return False


def extract_text(cfg: Config, db: Database, asset: dict, max_bytes: int = 2_000_000) -> str | None:
    """Return the (text) content of an asset, e.g. to read a prefab's YAML or a script."""
    pkg = db.package_by_id(asset["package_id"])
    for guid, leaf, size, mtime, fh in iter_package_members(cfg, pkg, {asset["guid"]}):
        if leaf == "asset":
            data = fh.read(max_bytes)
            return data.decode("utf-8", "replace")
    return None
