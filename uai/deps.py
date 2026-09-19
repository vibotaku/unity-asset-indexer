"""Transitive dependency resolution across the whole library (guid graph)."""
from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

from .db import Database, row_to_dict
from .unitypackage import is_builtin_guid

SCRIPT_KINDS = {"script"}

_KNOWN_GUIDS: dict[str, str] | None = None


def known_guid_label(guid: str) -> str | None:
    """Labels for well-known guids that live outside asset-store packages (Unity built-ins, UPM packages)."""
    global _KNOWN_GUIDS
    if _KNOWN_GUIDS is None:
        p = Path(__file__).with_name("known_guids.json")
        try:
            _KNOWN_GUIDS = {k.lower(): v for k, v in json.loads(p.read_text()).items()}
        except (OSError, json.JSONDecodeError):
            _KNOWN_GUIDS = {}
    if is_builtin_guid(guid):
        return "Unity built-in resource"
    return _KNOWN_GUIDS.get(guid.lower())


@dataclass
class Node:
    asset: dict
    depth: int
    via: str | None  # guid of the referrer that first pulled this in
    ambiguous_in: list[int] = field(default_factory=list)  # other package ids that also contain this guid


@dataclass
class Closure:
    roots: list[str]
    nodes: dict[str, Node]                       # guid -> Node (resolved assets, roots included)
    unresolved: dict[str, dict]                  # guid -> {"label": str|None, "referrers": [guid,...]}
    skipped_scripts: dict[str, dict]             # guid -> asset dict (when include_scripts=False)
    edges: dict[str, list[str]]                  # guid -> child guids (resolved or not)

    @property
    def packages(self) -> dict[int, list[Node]]:
        out: dict[int, list[Node]] = {}
        for n in self.nodes.values():
            out.setdefault(n.asset["package_id"], []).append(n)
        return out

    @property
    def total_bytes(self) -> int:
        return sum(n.asset["size"] or 0 for n in self.nodes.values())

    def scripts(self) -> list[Node]:
        return [n for n in self.nodes.values() if n.asset["kind"] in SCRIPT_KINDS]

    def to_dict(self) -> dict:
        return {
            "roots": self.roots,
            "assets": [
                {**n.asset, "depth": n.depth, "via": n.via, "also_in_packages": n.ambiguous_in}
                for n in sorted(self.nodes.values(), key=lambda n: (n.depth, n.asset["path"]))
            ],
            "unresolved": [{"guid": g, **v} for g, v in self.unresolved.items()],
            "skipped_scripts": list(self.skipped_scripts.values()),
            "total_bytes": self.total_bytes,
            "package_ids": sorted(self.packages.keys()),
        }


def _pick(candidates: list[dict], preferred_pkgs: list[int]) -> dict:
    for pid in preferred_pkgs:
        for c in candidates:
            if c["package_id"] == pid:
                return c
    return candidates[0]


def resolve_closure(db: Database, roots: list[dict], *, include_scripts: bool = True,
                    max_depth: int | None = None) -> Closure:
    """BFS over guid references starting from root asset rows (dicts from row_to_dict)."""
    nodes: dict[str, Node] = {}
    unresolved: dict[str, dict] = {}
    skipped_scripts: dict[str, dict] = {}
    edges: dict[str, list[str]] = {}
    queue: list[tuple[dict, int]] = []
    for r in roots:
        if r["guid"] in nodes:
            continue
        nodes[r["guid"]] = Node(asset=r, depth=0, via=None)
        queue.append((r, 0))

    selected_pkgs: list[int] = []

    def note_pkg(pid: int) -> None:
        if pid not in selected_pkgs:
            selected_pkgs.append(pid)

    for r in roots:
        note_pkg(r["package_id"])

    i = 0
    while i < len(queue):
        asset, depth = queue[i]
        i += 1
        if max_depth is not None and depth >= max_depth:
            continue
        children = db.refs_of(asset["id"])
        edges[asset["guid"]] = children
        for g in children:
            if g in nodes or g in skipped_scripts:
                continue
            if g in unresolved:
                unresolved[g]["referrers"].append(asset["guid"])
                continue
            cands = [row_to_dict(x) for x in db.assets_by_guid(g)]
            if not cands:
                unresolved[g] = {"label": known_guid_label(g), "referrers": [asset["guid"]]}
                continue
            chosen = _pick(cands, [asset["package_id"], *selected_pkgs])
            if not include_scripts and chosen["kind"] in SCRIPT_KINDS:
                skipped_scripts[g] = chosen
                continue
            others = [c["package_id"] for c in cands if c["package_id"] != chosen["package_id"]]
            nodes[g] = Node(asset=chosen, depth=depth + 1, via=asset["guid"], ambiguous_in=others)
            note_pkg(chosen["package_id"])
            queue.append((chosen, depth + 1))
    return Closure(roots=[r["guid"] for r in roots], nodes=nodes, unresolved=unresolved,
                   skipped_scripts=skipped_scripts, edges=edges)
