"""`uai` command line."""
from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path

from . import __version__
from .config import Config, load_config, save_library
from .db import Database, row_to_dict
from .deps import resolve_closure
from .exporter import (build_plan, cache_add, cache_list, cache_remove, export_to_dir, export_to_unitypackage,
                       extract_preview, extract_text, scan_project_guids)
from .indexer import index_library

GUID_RE = re.compile(r"^[0-9a-fA-F]{32}$")
KINDS = "prefab material shader texture model audio animation scene script asset font ui video doc data vfx folder other"


def human(n: int | None) -> str:
    n = n or 0
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{n:.0f}{unit}" if unit == "B" else f"{n:.1f}{unit}"
        n /= 1024
    return f"{n:.1f}TB"


def out_json(obj) -> None:
    json.dump(obj, sys.stdout, indent=2, default=str)
    sys.stdout.write("\n")


def err(msg: str, code: int = 1) -> None:
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(code)


class Ctx:
    def __init__(self, args):
        self.args = args
        self.cfg: Config = load_config(library=args.library, home=args.home)
        self.db = Database(self.cfg.db_path)

    def resolve_package(self, needle: str | None):
        if not needle:
            return None
        rows = self.db.find_packages(needle)
        if not rows:
            err(f"no package matches {needle!r}")
        if len(rows) > 1:
            names = "\n  ".join(f"[{r['id']}] {r['name']}" for r in rows)
            err(f"{needle!r} matches several packages; use the id or a longer name:\n  {names}")
        return rows[0]

    def resolve_asset(self, ident: str, package: str | None = None) -> dict:
        """Accept a guid, `package::path`, an exact `Assets/...` path, a path suffix, or a bare file name."""
        pkg = None
        if "::" in ident:
            pkg_name, ident = ident.split("::", 1)
            pkg = self.resolve_package(pkg_name)
        elif package:
            pkg = self.resolve_package(package)
        pid = pkg["id"] if pkg else None
        db = self.db
        if GUID_RE.match(ident):
            rows = db.assets_by_guid(ident)
            if pid is not None:
                rows = [r for r in rows if r["package_id"] == pid]
        else:
            ident = ident.replace("\\", "/").strip("/")
            rows = db.assets_by_path(ident, pid)
            if not rows:
                rows = db.assets_by_path_suffix(ident, pid)
            if not rows:
                rows = db.assets_by_name(ident, pid)
        if not rows:
            err(f"no asset matches {ident!r}. Try `uai search {ident.rsplit('/', 1)[-1]!r}`.")
        if len(rows) > 1:
            distinct = {(r["package_id"], r["guid"]) for r in rows}
            if len(distinct) > 1:
                listing = "\n  ".join(f"{r['guid']}  [{r['package']}] {r['path']}" for r in rows[:15])
                more = f"\n  ... {len(rows) - 15} more" if len(rows) > 15 else ""
                err(f"{ident!r} is ambiguous ({len(rows)} matches). Use the guid or `Package::Assets/path`:\n  {listing}{more}")
        return row_to_dict(rows[0])


# ----- commands -------------------------------------------------------------------------------------

def cmd_config(ctx: Ctx, a) -> None:
    if a.set_library:
        save_library(ctx.cfg, a.set_library)
        ctx.cfg.library = Path(a.set_library)
    info = {"library": str(ctx.cfg.library), "library_mounted": ctx.cfg.library.exists(), "home": str(ctx.cfg.home),
            "db": str(ctx.cfg.db_path), "cache_dir": str(ctx.cfg.cache_dir), **ctx.db.stats()}
    out_json(info) if a.json else print("\n".join(f"{k:16} {v}" for k, v in info.items()))


def cmd_index(ctx: Ctx, a) -> None:
    is_tty = sys.stderr.isatty()

    def log(s: str) -> None:
        if is_tty:
            sys.stderr.write("\r\033[K")
        print(s, flush=True)

    def progress(s: str) -> None:
        if is_tty:
            sys.stderr.write(f"\r\033[K  {s}")
            sys.stderr.flush()
        else:
            print(f"  {s}", flush=True)

    quiet = a.json
    res = index_library(ctx.cfg, ctx.db, workers=a.workers, force=a.force, only=a.only,
                        log=(lambda s: None) if quiet else log, progress=None if quiet else progress)
    if is_tty and not quiet:
        sys.stderr.write("\r\033[K")
    if a.json:
        out_json(res)
    else:
        print(f"done: {res['indexed']} indexed, {res['skipped']} up to date, {res['errors']} errors, "
              f"{res['entries']} assets in {res['seconds']:.0f}s")


def cmd_packages(ctx: Ctx, a) -> None:
    rows = [dict(r) for r in ctx.db.packages()]
    if a.json:
        out_json(rows)
        return
    print(f"{'id':>3}  {'assets':>7}  {'unpacked':>9}  {'package':>9}  {'version':>9}  {'unity':>11}  publisher / name  [category]")
    for r in rows:
        flag = "" if r["status"] == "ok" else f"  [{r['status']}]"
        print(f"{r['id']:>3}  {r['entry_count'] or 0:>7}  {human(r['total_bytes']):>9}  {human(r['size']):>9}  "
              f"{(r['version'] or ''):>9}  {(r['unity_version'] or ''):>11}  {r['publisher']} / {r['name']}"
              f"  [{r['category_label'] or r['category']}]{flag}")


def cmd_search(ctx: Ctx, a) -> None:
    rows = ctx.db.search(" ".join(a.query), kind=a.kind, package=a.package, publisher=a.publisher, ext=a.ext,
                         include_folders=a.folders, limit=a.limit)
    if a.json:
        out_json([row_to_dict(r) for r in rows])
        return
    if not rows:
        print("no matches")
        return
    for r in rows:
        print(f"{r['guid']}  {r['kind']:<9} {human(r['size']):>8}  [{r['package']}]  {r['path']}")
    print(f"-- {len(rows)} result(s){' (limit reached)' if len(rows) >= a.limit else ''}")


def cmd_ls(ctx: Ctx, a) -> None:
    pkg = ctx.resolve_package(a.package)
    rows = ctx.db.list_package_assets(pkg["id"], prefix=a.path, kind=a.kind, limit=a.limit)
    if a.json:
        out_json({"package": dict(pkg), "assets": [row_to_dict(r) for r in rows],
                  "kinds": [dict(k) for k in ctx.db.kind_counts(pkg["id"])]})
        return
    print(f"[{pkg['id']}] {pkg['publisher']} / {pkg['name']}  ({pkg['entry_count']} assets, {human(pkg['total_bytes'])})")
    for k in ctx.db.kind_counts(pkg["id"]):
        print(f"   {k['kind']:<10} {k['n']:>6}  {human(k['bytes'])}")
    if a.tree:
        _print_tree(rows)
    else:
        for r in rows:
            if r["is_folder"] and not a.folders:
                continue
            print(f"{r['guid']}  {r['kind']:<9} {human(r['size']):>8}  {r['path']}")


def _print_tree(rows) -> None:
    tree: dict = {}
    for r in rows:
        node = tree
        parts = r["path"].split("/")
        for p in parts[:-1]:
            node = node.setdefault(p + "/", {})
        if not r["is_folder"]:
            node[parts[-1]] = f"{human(r['size'])}"

    def walk(node, indent=0):
        for k in sorted(node, key=lambda x: (not x.endswith("/"), x.lower())):
            v = node[k]
            if isinstance(v, dict):
                print("  " * indent + k)
                walk(v, indent + 1)
            else:
                print("  " * indent + f"{k}  ({v})")
    walk(tree)


def cmd_info(ctx: Ctx, a) -> None:
    asset = ctx.resolve_asset(a.asset, a.package)
    refs = ctx.db.refs_of(asset["id"])
    referrers = [row_to_dict(r) for r in ctx.db.referrers(asset["guid"], limit=200)]
    others = [row_to_dict(r) for r in ctx.db.assets_by_guid(asset["guid"]) if r["id"] != asset["id"]]
    if a.json:
        out_json({**asset, "refs": refs, "referrers": referrers, "same_guid_in": others})
        return
    for k in ("guid", "path", "kind", "ext", "importer", "main_class", "package", "publisher"):
        print(f"{k:12} {asset.get(k)}")
    print(f"{'size':12} {human(asset['size'])}   preview: {'yes' if asset['has_preview'] else 'no'}   text: {'yes' if asset['is_text'] else 'no'}")
    if asset["labels"]:
        print(f"{'labels':12} {', '.join(asset['labels'])}")
    if others:
        print(f"{'also in':12} " + ", ".join(o["package"] for o in others))
    print(f"{'direct deps':12} {len(refs)}")
    for g in refs[:50]:
        cands = ctx.db.assets_by_guid(g)
        if cands:
            c = cands[0]
            print(f"   {g}  {c['kind']:<9} [{c['package']}] {c['path']}")
        else:
            from .deps import known_guid_label
            print(f"   {g}  (unresolved) {known_guid_label(g) or ''}")
    if len(refs) > 50:
        print(f"   ... {len(refs) - 50} more")
    print(f"{'used by':12} {len(referrers)}{' (showing 20)' if len(referrers) > 20 else ''}")
    for r in referrers[:20]:
        print(f"   {r['guid']}  {r['kind']:<9} [{r['package']}] {r['path']}")


def _roots(ctx: Ctx, idents: list[str], package: str | None) -> list[dict]:
    roots = []
    seen = set()
    for i in idents:
        r = ctx.resolve_asset(i, package)
        if (r["package_id"], r["guid"]) not in seen:
            seen.add((r["package_id"], r["guid"]))
            roots.append(r)
    return roots


def cmd_deps(ctx: Ctx, a) -> None:
    roots = _roots(ctx, a.assets, a.package)
    cl = resolve_closure(ctx.db, roots, include_scripts=not a.no_scripts, max_depth=a.depth)
    if a.json:
        out_json(cl.to_dict())
        return
    print(f"{len(cl.nodes)} asset(s) in closure, {human(cl.total_bytes)} total, "
          f"{len(cl.unresolved)} unresolved, {len(cl.skipped_scripts)} scripts skipped")
    if a.tree:
        _print_dep_tree(ctx, cl, roots, a.depth or 6)
    for pid, nodes in sorted(cl.packages.items()):
        pkg = ctx.db.package_by_id(pid)
        print(f"\n[{pid}] {pkg['publisher']} / {pkg['name']}  ({len(nodes)} assets, {human(sum(n.asset['size'] or 0 for n in nodes))})")
        for n in sorted(nodes, key=lambda n: (n.depth, n.asset["path"])):
            tag = "root" if n.depth == 0 else f"d{n.depth}"
            print(f"  {n.asset['guid']}  {tag:<4} {n.asset['kind']:<9} {human(n.asset['size']):>8}  {n.asset['path']}")
    if cl.skipped_scripts:
        print("\nscripts skipped (--no-scripts):")
        for s in cl.skipped_scripts.values():
            print(f"  {s['guid']}  [{s['package']}] {s['path']}")
    if cl.unresolved:
        print("\nunresolved guids (not in library; Unity built-ins / UPM packages / packages you do not have):")
        for g, v in cl.unresolved.items():
            ref = cl.nodes.get(v["referrers"][0])
            via = ref.asset["path"].rsplit("/", 1)[-1] if ref else v["referrers"][0]
            print(f"  {g}  {v['label'] or '?'}   (referenced by {via}{' +%d' % (len(v['referrers']) - 1) if len(v['referrers']) > 1 else ''})")


def _print_dep_tree(ctx: Ctx, cl, roots, depth: int) -> None:
    printed: set[str] = set()

    def walk(guid: str, indent: int) -> None:
        node = cl.nodes.get(guid)
        if node is None:
            u = cl.unresolved.get(guid)
            label = (u or {}).get("label") or "unresolved"
            print("  " * indent + f"?? {guid}  ({label})")
            return
        a = node.asset
        dup = " (see above)" if guid in printed else ""
        print("  " * indent + f"{a['kind']:<9} {a['path']}{dup}")
        if dup or indent >= depth:
            return
        printed.add(guid)
        for c in cl.edges.get(guid, []):
            if c in cl.skipped_scripts:
                print("  " * (indent + 1) + f"script    {cl.skipped_scripts[c]['path']} (skipped)")
                continue
            walk(c, indent + 1)

    for r in roots:
        walk(r["guid"], 0)
    print()


def cmd_rdeps(ctx: Ctx, a) -> None:
    asset = ctx.resolve_asset(a.asset, a.package)
    rows = [row_to_dict(r) for r in ctx.db.referrers(asset["guid"], limit=a.limit)]
    if a.json:
        out_json(rows)
        return
    print(f"{len(rows)} asset(s) reference {asset['path']}")
    for r in rows:
        print(f"  {r['guid']}  {r['kind']:<9} [{r['package']}] {r['path']}")


def cmd_export(ctx: Ctx, a) -> None:
    targets = [x for x in (a.project, a.out, a.unitypackage) if x]
    if len(targets) != 1:
        err("choose exactly one destination: --project <UnityProject> | --out <dir> | --unitypackage <file>")
    roots = _roots(ctx, a.assets, a.package)
    plan = build_plan(ctx.db, roots, include_deps=not a.no_deps, include_scripts=not a.no_scripts,
                      include_folders=not a.no_folders)
    log = (lambda s: None) if a.json or a.quiet else (lambda s: print(s, file=sys.stderr))
    project_guids: dict[str, str] = {}
    if a.project:
        proj = Path(a.project).expanduser().resolve()
        if not (proj / "Assets").is_dir() or not (proj / "ProjectSettings").is_dir():
            err(f"{proj} does not look like a Unity project (needs Assets/ and ProjectSettings/)")
        if not a.no_conflict_check:
            project_guids = scan_project_guids(proj)
        res = export_to_dir(ctx.cfg, ctx.db, plan, proj, force=a.force, dry_run=a.dry_run,
                            project_guids=project_guids, log=log)
    elif a.out:
        res = export_to_dir(ctx.cfg, ctx.db, plan, Path(a.out).expanduser(), force=a.force, dry_run=a.dry_run, log=log)
    else:
        res = export_to_unitypackage(ctx.cfg, ctx.db, plan, Path(a.unitypackage).expanduser(), dry_run=a.dry_run, log=log)
    payload = {
        "roots": [{"guid": r["guid"], "path": r["path"], "package": r["package"]} for r in roots],
        "planned": len(plan.assets), "planned_bytes": plan.total_bytes,
        "packages": [ctx.db.package_by_id(pid)["name"] for pid in plan.by_package],
        "warnings": plan.warnings,
        "unresolved": [{"guid": g, **v} for g, v in plan.closure.unresolved.items()],
        "scripts": [n.asset["path"] for n in plan.closure.scripts()],
        "skipped_scripts": [s["path"] for s in plan.closure.skipped_scripts.values()],
        "result": res.to_dict(),
    }
    if a.json:
        out_json(payload)
        return
    verb = "would write" if a.dry_run else "wrote"
    print(f"{verb} {len(res.written)} file(s) ({human(res.bytes_written)}) to {res.output} in {res.seconds:.1f}s")
    if not a.quiet:
        for w in res.written[: (len(res.written) if a.verbose else 40)]:
            print(f"  + {w['path']}")
        if len(res.written) > 40 and not a.verbose:
            print(f"  ... {len(res.written) - 40} more (use -v)")
    if res.skipped_existing:
        print(f"skipped {len(res.skipped_existing)} existing file(s) (use --force to overwrite)")
    if res.conflicts:
        print(f"CONFLICT: {len(res.conflicts)} guid(s) already exist in the project at a different path:")
        for c in res.conflicts:
            print(f"  {c['guid']}  {c['path']}  ->  already at {c['existing_path']}")
    if res.missing_in_package:
        print(f"WARNING: {len(res.missing_in_package)} planned asset(s) were not found inside the package (index stale? run `uai index`)")
    for w in plan.warnings:
        print(f"note: {w}")
    if plan.closure.unresolved and not a.quiet:
        print("unresolved references:")
        for g, v in list(plan.closure.unresolved.items())[:20]:
            print(f"  {g}  {v['label'] or '?'}")
        if len(plan.closure.unresolved) > 20:
            print(f"  ... {len(plan.closure.unresolved) - 20} more (see `uai deps`)")


def cmd_preview(ctx: Ctx, a) -> None:
    asset = ctx.resolve_asset(a.asset, a.package)
    if not asset["has_preview"]:
        err(f"{asset['path']} has no preview.png in its package")
    out = Path(a.out) if a.out else Path(f"{asset['name']}.preview.png")
    ok = extract_preview(ctx.cfg, ctx.db, asset, out)
    if not ok:
        err("preview not found in package")
    print(out if not a.json else json.dumps({"path": str(out)}))


def cmd_cat(ctx: Ctx, a) -> None:
    asset = ctx.resolve_asset(a.asset, a.package)
    if not asset["is_text"]:
        err(f"{asset['path']} is binary ({human(asset['size'])}); use `uai export`")
    text = extract_text(ctx.cfg, ctx.db, asset, max_bytes=a.max_bytes)
    if text is None:
        err("asset not found in package")
    sys.stdout.write(text)
    if not text.endswith("\n"):
        sys.stdout.write("\n")


def cmd_cache(ctx: Ctx, a) -> None:
    if a.action == "ls":
        rows = cache_list(ctx.cfg, ctx.db)
        if a.json:
            out_json(rows)
        else:
            for r in rows:
                print(f"[{r['package_id']}] {r['package']:<50} {human(r['bytes']):>9}  {'ok' if r['complete'] else 'partial'}")
            if not rows:
                print("cache is empty")
        return
    if not a.package:
        err("package required")
    pkg = ctx.resolve_package(a.package)
    if a.action == "add":
        p = cache_add(ctx.cfg, pkg, log=print)
        print(p)
    elif a.action == "rm":
        print("removed" if cache_remove(ctx.cfg, pkg) else "not cached")


def cmd_mcp(ctx: Ctx, a) -> None:
    from .mcp_server import main as mcp_main
    ctx.db.close()
    mcp_main()


# ----- parser ----------------------------------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="uai", description="Search and extract individual assets from a library of .unitypackage files.")
    p.add_argument("--library", help="library root (default: config / $UAI_LIBRARY)")
    p.add_argument("--home", help="index/cache dir (default: ~/.unity-asset-index or $UAI_HOME)")
    p.add_argument("--version", action="version", version=__version__)
    sub = p.add_subparsers(dest="cmd", required=True)

    def add_json(sp):
        sp.add_argument("--json", action="store_true", help="machine-readable output")

    s = sub.add_parser("config", help="show settings and index stats")
    s.add_argument("--library", dest="set_library", help="persist a new library root")
    add_json(s); s.set_defaults(fn=cmd_config)

    s = sub.add_parser("index", help="(re)index the library; incremental by size+mtime")
    s.add_argument("--workers", type=int, default=min(6, os.cpu_count() or 2))
    s.add_argument("--force", action="store_true", help="re-scan packages even if unchanged")
    s.add_argument("--only", help="only packages whose relative path contains this substring")
    add_json(s); s.set_defaults(fn=cmd_index)

    s = sub.add_parser("packages", help="list indexed packages")
    add_json(s); s.set_defaults(fn=cmd_packages)

    s = sub.add_parser("search", help="full-text search over asset paths")
    s.add_argument("query", nargs="*", help="words (prefix-matched; camelCase and _ split)")
    s.add_argument("-k", "--kind", help=f"comma list of: {KINDS}")
    s.add_argument("-e", "--ext", help="file extension, e.g. fbx")
    s.add_argument("-p", "--package", help="restrict to packages whose name contains this")
    s.add_argument("-P", "--publisher")
    s.add_argument("-n", "--limit", type=int, default=50)
    s.add_argument("--folders", action="store_true", help="include folder entries")
    add_json(s); s.set_defaults(fn=cmd_search)

    s = sub.add_parser("ls", help="list a package's contents")
    s.add_argument("package", help="package id or (partial) name")
    s.add_argument("--path", help="only under this Assets/... prefix")
    s.add_argument("-k", "--kind")
    s.add_argument("-n", "--limit", type=int)
    s.add_argument("--tree", action="store_true")
    s.add_argument("--folders", action="store_true")
    add_json(s); s.set_defaults(fn=cmd_ls)

    ident_help = "guid | Package::Assets/path | Assets/path | path suffix | file name"
    s = sub.add_parser("info", help="details, direct dependencies and users of one asset")
    s.add_argument("asset", help=ident_help)
    s.add_argument("-p", "--package")
    add_json(s); s.set_defaults(fn=cmd_info)

    s = sub.add_parser("deps", help="transitive dependency closure (cross-package)")
    s.add_argument("assets", nargs="+", help=ident_help)
    s.add_argument("-p", "--package")
    s.add_argument("--depth", type=int, help="limit traversal depth")
    s.add_argument("--no-scripts", action="store_true", help="do not follow into scripts/plugins")
    s.add_argument("--tree", action="store_true", help="also print as a tree")
    add_json(s); s.set_defaults(fn=cmd_deps)

    s = sub.add_parser("rdeps", help="assets that reference this asset")
    s.add_argument("asset", help=ident_help)
    s.add_argument("-p", "--package")
    s.add_argument("-n", "--limit", type=int, default=200)
    add_json(s); s.set_defaults(fn=cmd_rdeps)

    s = sub.add_parser("export", help="extract assets + dependencies into a Unity project, folder or .unitypackage")
    s.add_argument("assets", nargs="+", help=ident_help)
    s.add_argument("-p", "--package")
    s.add_argument("--project", help="Unity project dir (writes Assets/... with .meta files)")
    s.add_argument("--out", help="plain output dir")
    s.add_argument("--unitypackage", help="write a slim .unitypackage instead")
    s.add_argument("--no-deps", action="store_true")
    s.add_argument("--no-scripts", action="store_true", help="leave scripts/plugins out of the dependency set")
    s.add_argument("--no-folders", action="store_true", help="do not write folder .meta files")
    s.add_argument("--no-conflict-check", action="store_true", help="skip scanning project .meta guids")
    s.add_argument("--force", action="store_true", help="overwrite existing files")
    s.add_argument("--dry-run", action="store_true")
    s.add_argument("-q", "--quiet", action="store_true")
    s.add_argument("-v", "--verbose", action="store_true")
    add_json(s); s.set_defaults(fn=cmd_export)

    s = sub.add_parser("preview", help="extract an asset's preview.png")
    s.add_argument("asset", help=ident_help)
    s.add_argument("-p", "--package")
    s.add_argument("-o", "--out")
    add_json(s); s.set_defaults(fn=cmd_preview)

    s = sub.add_parser("cat", help="print a text asset (prefab YAML, script, shader...)")
    s.add_argument("asset", help=ident_help)
    s.add_argument("-p", "--package")
    s.add_argument("--max-bytes", type=int, default=2_000_000)
    s.set_defaults(fn=cmd_cat)

    s = sub.add_parser("cache", help="keep a fully-extracted local copy of a package for fast exports")
    s.add_argument("action", choices=["add", "rm", "ls"])
    s.add_argument("package", nargs="?")
    add_json(s); s.set_defaults(fn=cmd_cache)

    s = sub.add_parser("mcp", help="run the MCP server (stdio) for agents")
    s.set_defaults(fn=cmd_mcp)
    return p


def main(argv: list[str] | None = None) -> None:
    args = build_parser().parse_args(argv)
    ctx = Ctx(args)
    try:
        args.fn(ctx, args)
    except KeyboardInterrupt:
        sys.exit(130)
    except BrokenPipeError:
        sys.exit(0)
    finally:
        try:
            ctx.db.close()
        except Exception:
            pass


if __name__ == "__main__":
    main()
