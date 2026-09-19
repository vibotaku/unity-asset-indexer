"""End-to-end tests against a synthetic .unitypackage (no network share needed)."""
from __future__ import annotations

import gzip
import io
import json
import os
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from uai.config import Config  # noqa: E402
from uai.db import Database, row_to_dict  # noqa: E402
from uai.deps import resolve_closure  # noqa: E402
from uai.exporter import build_plan, cache_add, export_to_dir, export_to_unitypackage  # noqa: E402
from uai.indexer import index_library  # noqa: E402
from uai.unitypackage import iter_package, read_package_header  # noqa: E402

G = {
    "prefab": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1",
    "mat": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2",
    "tex": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3",
    "fbx": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa4",
    "script": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa5",
    "folder": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6",
    "other_pkg_tex": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1",
    "missing": "cccccccccccccccccccccccccccccccc",
}
URP_LIT = "933532a4fcc9baf4fa0491de14d08ed7"


def _meta(guid: str, importer: str, folder: bool = False, extra: str = "") -> bytes:
    body = f"fileFormatVersion: 2\nguid: {guid}\n"
    if folder:
        body += "folderAsset: yes\n"
    body += f"{importer}:\n  externalObjects: {{}}\n{extra}  userData: \n"
    return body.encode()


def make_package(path: Path, entries: list[tuple[str, str, bytes | None, bytes, bool]], header: dict | None = None) -> None:
    """entries: (guid, pathname, asset bytes or None, meta bytes, has_preview)"""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        for guid, pathname, asset, meta, preview in entries:
            d = tarfile.TarInfo(guid); d.type = tarfile.DIRTYPE; tar.addfile(d)

            def add(name: str, data: bytes) -> None:
                ti = tarfile.TarInfo(f"{guid}/{name}"); ti.size = len(data); tar.addfile(ti, io.BytesIO(data))
            if asset is not None:
                add("asset", asset)
            add("asset.meta", meta)
            add("pathname", (pathname + "\n00").encode())
            if preview:
                add("preview.png", b"\x89PNG\r\n\x1a\nfakepng")
    raw = buf.getvalue()
    if header is None:
        path.write_bytes(gzip.compress(raw))
        return
    # Emulate the Asset Store gzip FEXTRA header (this is what breaks tarfile's own r|gz mode).
    payload = json.dumps(header).encode()
    sub = b"A$" + struct.pack("<H", len(payload)) + payload
    gz = gzip.compress(raw)
    hdr = bytearray(gz[:10]); hdr[3] |= 4
    path.write_bytes(bytes(hdr) + struct.pack("<H", len(sub)) + sub + gz[10:])


PREFAB_YAML = f"""%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_Name: Chest
  m_Component:
  - component: {{fileID: 200}}
--- !u!23 &200
MeshRenderer:
  m_Materials:
  - {{fileID: 2100000, guid: {G['mat']}, type: 2}}
--- !u!33 &300
MeshFilter:
  m_Mesh: {{fileID: 4300000, guid: {G['fbx']}, type: 3}}
--- !u!114 &400
MonoBehaviour:
  m_Script: {{fileID: 11500000, guid: {G['script']}, type: 3}}
  someMissing: {{fileID: 1, guid: {G['missing']}, type: 2}}
  builtin: {{fileID: 10754, guid: 0000000000000000f000000000000000, type: 0}}
"""
MAT_YAML = f"""%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!21 &2100000
Material:
  m_Name: Chest
  m_Shader: {{fileID: 4800000, guid: {URP_LIT}, type: 3}}
  m_SavedProperties:
    m_TexEnvs:
    - _BaseMap:
        m_Texture: {{fileID: 2800000, guid: {G['tex']}, type: 3}}
    - _Detail:
        m_Texture: {{fileID: 2800000, guid: {G['other_pkg_tex']}, type: 3}}
"""


class UaiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        root = Path(cls.tmp.name)
        cls.lib = root / "lib"
        (cls.lib / "Pub A" / "3D ModelsProps").mkdir(parents=True)
        (cls.lib / "Pub B" / "Textures").mkdir(parents=True)
        make_package(
            cls.lib / "Pub A" / "3D ModelsProps" / "Chest Pack.unitypackage",
            [
                (G["folder"], "Assets/Chest", None, _meta(G["folder"], "DefaultImporter", folder=True), False),
                (G["prefab"], "Assets/Chest/Prefabs/Chest.prefab", PREFAB_YAML.encode(), _meta(G["prefab"], "PrefabImporter"), True),
                (G["mat"], "Assets/Chest/Materials/Chest.mat", MAT_YAML.encode(), _meta(G["mat"], "NativeFormatImporter"), False),
                (G["tex"], "Assets/Chest/Textures/Chest_Albedo.png", b"\x89PNG\x00\x00binary" * 100, _meta(G["tex"], "TextureImporter"), False),
                (G["fbx"], "Assets/Chest/Models/SM_Chest.fbx", b"Kaydara FBX Binary\x00\x00" * 50, _meta(G["fbx"], "ModelImporter"), False),
                (G["script"], "Assets/Chest/Scripts/ChestOpener.cs", b"using UnityEngine;\npublic class ChestOpener : MonoBehaviour {}\n", _meta(G["script"], "MonoImporter"), False),
            ],
            header={"title": "Chest Pack", "version": "1.2.3", "unity_version": "2022.3.1f1", "id": "12345",
                    "category": {"id": "1", "label": "3D Models/Props"}, "publisher": {"id": "9", "label": "Pub A"}},
        )
        make_package(
            cls.lib / "Pub B" / "Textures" / "Shared Textures.unitypackage",
            [(G["other_pkg_tex"], "Assets/Shared/Detail.png", b"\x89PNGbinary", _meta(G["other_pkg_tex"], "TextureImporter"), False)],
        )
        cls.home = root / "home"
        cls.cfg = Config(library=cls.lib, home=cls.home)
        cls.db = Database(cls.cfg.db_path)
        cls.result = index_library(cls.cfg, cls.db, workers=1, log=lambda s: None)

    @classmethod
    def tearDownClass(cls):
        cls.db.close()
        cls.tmp.cleanup()

    def test_reader_handles_fextra_header(self):
        p = self.lib / "Pub A" / "3D ModelsProps" / "Chest Pack.unitypackage"
        entries = {e.guid: e for e in iter_package(str(p))}
        self.assertEqual(len(entries), 6)
        self.assertEqual(entries[G["prefab"]].path, "Assets/Chest/Prefabs/Chest.prefab")
        self.assertTrue(entries[G["folder"]].is_folder)
        self.assertTrue(entries[G["prefab"]].has_preview)
        self.assertEqual(entries[G["prefab"]].main_class, "GameObject")
        self.assertEqual(entries[G["mat"]].main_class, "Material")
        # built-in guid dropped, self guid dropped, others kept
        self.assertEqual(entries[G["prefab"]].refs, {G["mat"], G["fbx"], G["script"], G["missing"]})
        self.assertFalse(entries[G["tex"]].is_text)
        self.assertEqual(read_package_header(str(p))["title"], "Chest Pack")

    def test_index_and_metadata(self):
        self.assertEqual(self.result["indexed"], 2)
        self.assertEqual(self.result["errors"], 0)
        pkg = self.db.find_packages("Chest Pack")[0]
        self.assertEqual(pkg["version"], "1.2.3")
        self.assertEqual(pkg["category_label"], "3D Models/Props")
        self.assertEqual(pkg["entry_count"], 6)
        # incremental: second run skips everything
        again = index_library(self.cfg, self.db, workers=1, log=lambda s: None)
        self.assertEqual(again["indexed"], 0)
        self.assertEqual(again["skipped"], 2)

    def test_search(self):
        rows = self.db.search("chest", kind="prefab")
        self.assertEqual([r["guid"] for r in rows], [G["prefab"]])
        rows = self.db.search("sm chest")  # camelCase / underscore split
        self.assertIn(G["fbx"], [r["guid"] for r in rows])
        rows = self.db.search("albedo", publisher="Pub A")
        self.assertEqual(rows[0]["guid"], G["tex"])
        self.assertEqual(self.db.search("albedo", publisher="Pub B"), [])

    def test_closure_cross_package_and_unresolved(self):
        root = row_to_dict(self.db.assets_by_guid(G["prefab"])[0])
        cl = resolve_closure(self.db, [root])
        self.assertEqual(set(cl.nodes), {G["prefab"], G["mat"], G["tex"], G["fbx"], G["script"], G["other_pkg_tex"]})
        self.assertEqual(cl.nodes[G["tex"]].depth, 2)
        self.assertEqual(cl.nodes[G["tex"]].via, G["mat"])
        self.assertEqual(len(cl.packages), 2)
        self.assertIn(G["missing"], cl.unresolved)
        self.assertIn(URP_LIT, cl.unresolved)
        self.assertIn("URP", cl.unresolved[URP_LIT]["label"])
        cl2 = resolve_closure(self.db, [root], include_scripts=False)
        self.assertNotIn(G["script"], cl2.nodes)
        self.assertIn(G["script"], cl2.skipped_scripts)
        self.assertEqual(len(self.db.referrers(G["mat"])), 1)

    def test_export_to_project_with_folders_and_conflicts(self):
        proj = Path(self.tmp.name) / "Proj"
        (proj / "Assets" / "Elsewhere").mkdir(parents=True)
        (proj / "ProjectSettings").mkdir()
        # Pre-existing asset in the project with the *texture's* guid at another path -> conflict.
        (proj / "Assets" / "Elsewhere" / "Old.png").write_bytes(b"x")
        (proj / "Assets" / "Elsewhere" / "Old.png.meta").write_bytes(_meta(G["tex"], "TextureImporter"))
        from uai.exporter import scan_project_guids
        root = row_to_dict(self.db.assets_by_guid(G["prefab"])[0])
        plan = build_plan(self.db, [root])
        self.assertTrue(any(a["is_folder"] for a in plan.assets))
        res = export_to_dir(self.cfg, self.db, plan, proj, project_guids=scan_project_guids(proj))
        written = {w["path"] for w in res.written}
        self.assertIn("Assets/Chest/Prefabs/Chest.prefab", written)
        self.assertIn("Assets/Shared/Detail.png", written)  # cross-package dep
        self.assertIn("Assets/Chest", written)  # folder meta
        self.assertTrue((proj / "Assets/Chest.meta").is_file())
        self.assertTrue((proj / "Assets/Chest/Prefabs/Chest.prefab.meta").is_file())
        self.assertEqual([c["guid"] for c in res.conflicts], [G["tex"]])
        self.assertFalse((proj / "Assets/Chest/Textures/Chest_Albedo.png").exists())
        self.assertEqual(res.missing_in_package, [])
        # Re-export: everything already there is skipped, nothing rewritten.
        res2 = export_to_dir(self.cfg, self.db, plan, proj, project_guids=scan_project_guids(proj))
        self.assertEqual(res2.written, [])
        self.assertGreater(len(res2.skipped_existing), 0)

    def test_export_unitypackage_roundtrip_and_cache(self):
        root = row_to_dict(self.db.assets_by_guid(G["mat"])[0])
        plan = build_plan(self.db, [root])
        out = Path(self.tmp.name) / "slim.unitypackage"
        res = export_to_unitypackage(self.cfg, self.db, plan, out)
        self.assertTrue(out.is_file())
        self.assertEqual(res.missing_in_package, [])
        names = set()
        with tarfile.open(out, "r:gz") as tar:
            names = {m.name for m in tar.getmembers()}
        self.assertIn(f"{G['mat']}/asset", names)
        self.assertIn(f"{G['mat']}/asset.meta", names)
        self.assertIn(f"{G['mat']}/pathname", names)
        self.assertIn(f"{G['other_pkg_tex']}/asset", names)
        # now cache a package and export again from the cache
        pkg = self.db.find_packages("Chest Pack")[0]
        cache_add(self.cfg, pkg, log=lambda s: None)
        out2 = Path(self.tmp.name) / "slim2.unitypackage"
        res2 = export_to_unitypackage(self.cfg, self.db, plan, out2)
        self.assertEqual({w["guid"] for w in res2.written}, {w["guid"] for w in res.written})

    def test_cli_json(self):
        env = {**os.environ, "UAI_HOME": str(self.home), "UAI_LIBRARY": str(self.lib)}
        out = subprocess.run([sys.executable, "-m", "uai", "search", "chest", "--json"], capture_output=True, text=True, env=env, cwd=ROOT)
        self.assertEqual(out.returncode, 0, out.stderr)
        data = json.loads(out.stdout)
        self.assertTrue(any(d["guid"] == G["prefab"] for d in data))
        out = subprocess.run([sys.executable, "-m", "uai", "deps", "Chest.prefab", "--json"], capture_output=True, text=True, env=env, cwd=ROOT)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(json.loads(out.stdout)["roots"], [G["prefab"]])
        out = subprocess.run([sys.executable, "-m", "uai", "export", "Chest.prefab", "--out", str(Path(self.tmp.name) / "cli_out"), "--json", "--no-scripts"], capture_output=True, text=True, env=env, cwd=ROOT)
        self.assertEqual(out.returncode, 0, out.stderr)
        payload = json.loads(out.stdout)
        self.assertEqual(payload["skipped_scripts"], ["Assets/Chest/Scripts/ChestOpener.cs"])
        self.assertTrue((Path(self.tmp.name) / "cli_out" / "Assets/Chest/Prefabs/Chest.prefab").is_file())


if __name__ == "__main__":
    unittest.main()
