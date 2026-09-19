"""MCP (stdio) server exposing the asset index to agents.

Run with `uai mcp` or `uai-mcp`. Requires the optional `mcp` dependency: `uv pip install -e '.[mcp]'`.
Register in Claude Code with:
    claude mcp add unity-assets -- /path/to/.venv/bin/uai-mcp
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any

try:  # mcp >= 2.0
    from mcp.server.mcpserver import MCPServer as FastMCP
except ImportError:  # pragma: no cover
    try:  # mcp 1.x
        from mcp.server.fastmcp import FastMCP
    except ImportError as e:
        raise SystemExit("The MCP server needs the `mcp` package: uv pip install -e '.[mcp]'") from e

from .config import load_config
from .db import Database, row_to_dict
from .deps import resolve_closure
from .exporter import build_plan, export_to_dir, export_to_unitypackage, extract_text, scan_project_guids

mcp = FastMCP(
    "unity-asset-index",
    instructions=(
        "Search a library of Unity .unitypackage files by asset name, inspect an asset's dependency closure "
        "(prefab -> mesh/material/texture/shader/script, across packages), and export exactly those files into a "
        "Unity project's Assets/ folder with their .meta files so guids stay intact. Typical flow: search_assets -> "
        "dependencies (optional) -> export_assets(project_dir=...). Identifiers accept a guid, 'Package::Assets/path', "
        "an Assets/ path, a path suffix or a bare file name."
    ),
)

_cfg = load_config()


def _db() -> Database:
    return Database(_cfg.db_path)


def _resolve(db: Database, ident: str, package: str | None = None) -> dict:
    import re
    pkg_id = None
    if "::" in ident:
        pkg_name, ident = ident.split("::", 1)
        pkgs = db.find_packages(pkg_name)
        if len(pkgs) != 1:
            raise ValueError(f"package {pkg_name!r} matched {len(pkgs)} packages")
        pkg_id = pkgs[0]["id"]
    elif package:
        pkgs = db.find_packages(package)
        if len(pkgs) != 1:
            raise ValueError(f"package {package!r} matched {len(pkgs)} packages")
        pkg_id = pkgs[0]["id"]
    if re.fullmatch(r"[0-9a-fA-F]{32}", ident):
        rows = db.assets_by_guid(ident)
        if pkg_id is not None:
            rows = [r for r in rows if r["package_id"] == pkg_id]
    else:
        ident = ident.replace("\\", "/").strip("/")
        rows = db.assets_by_path(ident, pkg_id) or db.assets_by_path_suffix(ident, pkg_id) or db.assets_by_name(ident, pkg_id)
    if not rows:
        raise ValueError(f"no asset matches {ident!r}; use search_assets first")
    distinct = {(r["package_id"], r["guid"]) for r in rows}
    if len(distinct) > 1:
        cands = [f"{r['guid']} [{r['package']}] {r['path']}" for r in rows[:10]]
        raise ValueError(f"{ident!r} is ambiguous; use a guid or Package::path. Candidates: " + "; ".join(cands))
    return row_to_dict(rows[0])


@mcp.tool()
def list_packages() -> list[dict[str, Any]]:
    """List every indexed .unitypackage (id, name, publisher, category, version, asset count, sizes)."""
    db = _db()
    try:
        keys = ("id", "name", "title", "publisher", "category_label", "version", "unity_version", "pubdate",
                "entry_count", "total_bytes", "size", "status", "description")
        return [{k: r[k] for k in keys} for r in db.packages()]
    finally:
        db.close()


@mcp.tool()
def search_assets(query: str, kind: str | None = None, package: str | None = None, publisher: str | None = None,
                  ext: str | None = None, limit: int = 30) -> list[dict[str, Any]]:
    """Full-text search over asset paths in all packages.

    query: words, prefix-matched (camelCase/underscores are split: 'sm env lily' finds SM_Env_Lily_01).
    kind: comma list of prefab, material, shader, texture, model, audio, animation, scene, script, asset, font, ui, vfx.
    package / publisher: substring filters. ext: e.g. 'fbx'.
    Returns guid, path, kind, size (bytes), package, has_preview, is_text.
    """
    db = _db()
    try:
        rows = db.search(query, kind=kind, package=package, publisher=publisher, ext=ext, limit=limit)
        keys = ("guid", "path", "name", "kind", "ext", "main_class", "size", "package", "package_id", "publisher",
                "has_preview", "is_text")
        return [{k: row_to_dict(r)[k] for k in keys} for r in rows]
    finally:
        db.close()


@mcp.tool()
def list_package_assets(package: str, path_prefix: str | None = None, kind: str | None = None, limit: int = 500) -> dict[str, Any]:
    """List assets inside one package (by id or partial name), optionally under an Assets/... prefix or of one kind."""
    db = _db()
    try:
        pkgs = db.find_packages(package)
        if len(pkgs) != 1:
            return {"error": f"{package!r} matched {len(pkgs)} packages", "candidates": [dict(p)["name"] for p in pkgs[:20]]}
        pkg = pkgs[0]
        rows = db.list_package_assets(pkg["id"], prefix=path_prefix, kind=kind, limit=limit)
        return {
            "package": {"id": pkg["id"], "name": pkg["name"], "publisher": pkg["publisher"], "entry_count": pkg["entry_count"]},
            "kinds": [dict(k) for k in db.kind_counts(pkg["id"])],
            "assets": [{"guid": r["guid"], "path": r["path"], "kind": r["kind"], "size": r["size"]} for r in rows if not r["is_folder"]],
        }
    finally:
        db.close()


@mcp.tool()
def asset_info(identifier: str, package: str | None = None) -> dict[str, Any]:
    """Details for one asset: metadata, direct dependencies (resolved), and assets that use it."""
    db = _db()
    try:
        a = _resolve(db, identifier, package)
        refs = []
        for g in db.refs_of(a["id"]):
            c = db.assets_by_guid(g)
            if c:
                refs.append({"guid": g, "path": c[0]["path"], "kind": c[0]["kind"], "package": c[0]["package"]})
            else:
                from .deps import known_guid_label
                refs.append({"guid": g, "unresolved": True, "label": known_guid_label(g)})
        users = [{"guid": r["guid"], "path": r["path"], "kind": r["kind"], "package": r["package"]}
                 for r in db.referrers(a["guid"], limit=100)]
        return {**a, "direct_dependencies": refs, "used_by": users}
    except ValueError as e:
        return {"error": str(e)}
    finally:
        db.close()


@mcp.tool()
def dependencies(identifiers: list[str], include_scripts: bool = True, max_depth: int | None = None) -> dict[str, Any]:
    """Transitive dependency closure for one or more assets, across packages.

    Returns every asset that would be needed (with depth and the referrer that pulled it in), unresolved guids
    (Unity built-ins, UPM packages, or packages not in the library), and total bytes.
    """
    db = _db()
    try:
        roots = [_resolve(db, i) for i in identifiers]
        cl = resolve_closure(db, roots, include_scripts=include_scripts, max_depth=max_depth)
        d = cl.to_dict()
        d["packages"] = [{"id": pid, "name": db.package_by_id(pid)["name"]} for pid in d["package_ids"]]
        return d
    except ValueError as e:
        return {"error": str(e)}
    finally:
        db.close()


@mcp.tool()
def export_assets(identifiers: list[str], project_dir: str | None = None, out_dir: str | None = None,
                  unitypackage_path: str | None = None, include_deps: bool = True, include_scripts: bool = True,
                  force: bool = False, dry_run: bool = False) -> dict[str, Any]:
    """Extract assets (+ transitive dependencies) with their .meta files.

    Exactly one destination: project_dir (a Unity project; files land under its Assets/ at their original paths and
    guid conflicts with existing project assets are detected), out_dir (plain folder), or unitypackage_path (a slim
    .unitypackage). Existing files are skipped unless force=True. Use dry_run=True to preview.
    """
    targets = [t for t in (project_dir, out_dir, unitypackage_path) if t]
    if len(targets) != 1:
        return {"error": "pass exactly one of project_dir, out_dir, unitypackage_path"}
    db = _db()
    try:
        roots = [_resolve(db, i) for i in identifiers]
        plan = build_plan(db, roots, include_deps=include_deps, include_scripts=include_scripts)
        if project_dir:
            proj = Path(project_dir).expanduser().resolve()
            if not (proj / "Assets").is_dir() or not (proj / "ProjectSettings").is_dir():
                return {"error": f"{proj} is not a Unity project (needs Assets/ and ProjectSettings/)"}
            res = export_to_dir(_cfg, db, plan, proj, force=force, dry_run=dry_run, project_guids=scan_project_guids(proj))
        elif out_dir:
            res = export_to_dir(_cfg, db, plan, Path(out_dir).expanduser(), force=force, dry_run=dry_run)
        else:
            res = export_to_unitypackage(_cfg, db, plan, Path(unitypackage_path).expanduser(), dry_run=dry_run)
        return {
            "roots": [{"guid": r["guid"], "path": r["path"], "package": r["package"]} for r in roots],
            "planned": len(plan.assets), "planned_bytes": plan.total_bytes,
            "packages": [db.package_by_id(pid)["name"] for pid in plan.by_package],
            "warnings": plan.warnings,
            "unresolved": [{"guid": g, **v} for g, v in plan.closure.unresolved.items()],
            "scripts": [n.asset["path"] for n in plan.closure.scripts()],
            "result": res.to_dict(),
        }
    except ValueError as e:
        return {"error": str(e)}
    finally:
        db.close()


@mcp.tool()
def read_text_asset(identifier: str, max_bytes: int = 200_000) -> dict[str, Any]:
    """Return the text of a YAML/script/shader asset (prefab structure, material properties, C# source...)."""
    db = _db()
    try:
        a = _resolve(db, identifier)
        if not a["is_text"]:
            return {"error": f"{a['path']} is binary"}
        return {"guid": a["guid"], "path": a["path"], "text": extract_text(_cfg, db, a, max_bytes=max_bytes)}
    except ValueError as e:
        return {"error": str(e)}
    finally:
        db.close()


def main() -> None:
    mcp.run()


if __name__ == "__main__":
    main()
