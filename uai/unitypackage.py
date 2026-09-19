"""Streaming reader for .unitypackage files.

A .unitypackage is a gzipped tar whose top-level directories are asset GUIDs:

    <guid>/asset         the asset bytes (absent for folders)
    <guid>/asset.meta    Unity .meta YAML (importer settings; contains the guid)
    <guid>/pathname      "Assets/Path/To/File.ext" (+ an optional second line)
    <guid>/preview.png   optional thumbnail

Dependencies between assets are expressed as `guid: <32 hex>` references inside
text (YAML/JSON) assets and inside .meta files. We never extract binaries while
indexing; we stream every package exactly once and record metadata + references.
"""
from __future__ import annotations

import contextlib
import gzip
import json
import re
import struct
import tarfile
from dataclasses import dataclass, field
from pathlib import PurePosixPath
from typing import BinaryIO, Iterable, Iterator

GUID_DIR_RE = re.compile(r"^[0-9a-fA-F]{32}$")
# Matches YAML `guid: abc...`, JSON `"guid": "abc..."` and escaped-JSON `\"guid\":\"abc...\"`.
GUID_REF_RE = re.compile(rb'guid[\\"]*\s*:\s*[\\"]*([0-9a-fA-F]{32})')
CLASS_ID_RE = re.compile(rb"^--- !u!(\d+)", re.M)
IMPORTER_RE = re.compile(r"^([A-Za-z0-9]+Importer):", re.M)
LABELS_RE = re.compile(r"^labels:\n((?:- .*\n?)+)", re.M)
BUILTIN_GUID_RE = re.compile(r"^0{16}[0-9a-f]0{15}$")  # ...e000... builtin resources, ...f000... builtin extra
NULL_GUID = "0" * 32

DEFAULT_BUFSIZE = 4 * 1024 * 1024
MAX_SCAN_BYTES = 256 * 1024 * 1024
HEAD_BYTES = 8192


class _CountingReader:
    """Wraps a binary file so every read adds to a shared counter (used for indexing progress)."""

    def __init__(self, f, counter):
        self._f = f
        self._c = counter

    def read(self, n: int = -1) -> bytes:
        b = self._f.read(n)
        if b:
            with self._c.get_lock():
                self._c.value += len(b)
        return b

    def readinto(self, buf) -> int:
        n = self._f.readinto(buf)
        if n:
            with self._c.get_lock():
                self._c.value += n
        return n

    def __getattr__(self, name):
        return getattr(self._f, name)


# Set per worker process by the indexer; None means "do not count".
PROGRESS_COUNTER = None


@contextlib.contextmanager
def open_stream(path: str, bufsize: int = DEFAULT_BUFSIZE):
    """Open a .unitypackage for sequential reading.

    tarfile's own "r|gz" mode mis-parses gzip headers that carry an FEXTRA field (CPython reads the
    extra bytes through the decompressor), and Asset Store packages always have one. Wrapping the file in
    gzip.GzipFile and handing tarfile a plain stream ("r|") avoids that and is just as fast.
    """
    with open(path, "rb", buffering=bufsize) as raw:
        src = _CountingReader(raw, PROGRESS_COUNTER) if PROGRESS_COUNTER is not None else raw
        with gzip.GzipFile(fileobj=src, mode="rb") as gz, tarfile.open(fileobj=gz, mode="r|", bufsize=bufsize) as tar:
            yield tar


def read_package_header(path: str) -> dict:
    """Asset Store packages embed JSON in the gzip extra field (store id, unity version, title, ...)."""
    try:
        with open(path, "rb") as f:
            head = f.read(65536)
    except OSError:
        return {}
    if len(head) < 12 or head[:2] != b"\x1f\x8b" or not (head[3] & 4):
        return {}
    xlen = struct.unpack("<H", head[10:12])[0]
    extra = head[12:12 + xlen]
    pos = 0
    while pos + 4 <= len(extra):
        ln = struct.unpack("<H", extra[pos + 2:pos + 4])[0]
        data = extra[pos + 4:pos + 4 + ln]
        pos += 4 + ln
        try:
            obj = json.loads(data.decode("utf-8", "replace"))
            if isinstance(obj, dict):
                return obj
        except (ValueError, UnicodeDecodeError):
            continue
    return {}

# Extension -> coarse kind used for filtering/search.
EXT_KIND = {
    "prefab": "prefab",
    "mat": "material",
    "shader": "shader", "shadergraph": "shader", "shadersubgraph": "shader", "hlsl": "shader",
    "cginc": "shader", "cg": "shader", "compute": "shader", "raytrace": "shader", "shadervariants": "shader",
    "png": "texture", "jpg": "texture", "jpeg": "texture", "tga": "texture", "psd": "texture", "psb": "texture",
    "tif": "texture", "tiff": "texture", "exr": "texture", "hdr": "texture", "bmp": "texture", "gif": "texture",
    "dds": "texture", "webp": "texture", "renderTexture": "texture", "cubemap": "texture",
    "fbx": "model", "obj": "model", "blend": "model", "dae": "model", "3ds": "model", "max": "model",
    "ma": "model", "mb": "model", "c4d": "model", "glb": "model", "gltf": "model", "ply": "model", "stl": "model",
    "wav": "audio", "mp3": "audio", "ogg": "audio", "aif": "audio", "aiff": "audio", "flac": "audio",
    "mod": "audio", "it": "audio", "s3m": "audio", "xm": "audio", "mixer": "audio",
    "anim": "animation", "controller": "animation", "overrideController": "animation", "mask": "animation",
    "playable": "animation", "signal": "animation",
    "unity": "scene",
    "cs": "script", "js": "script", "boo": "script", "dll": "script", "asmdef": "script", "asmref": "script",
    "pdb": "script", "mdb": "script", "so": "script", "dylib": "script", "bundle": "script", "aar": "script",
    "jar": "script", "a": "script", "mm": "script", "m": "script", "h": "script", "cpp": "script", "c": "script",
    "asset": "asset", "preset": "asset", "physicMaterial": "asset", "physicsMaterial2D": "asset",
    "terrainlayer": "asset", "brush": "asset", "flare": "asset", "guiskin": "asset", "lighting": "asset",
    "giparams": "asset", "spriteatlas": "asset", "spriteatlasv2": "asset", "inputactions": "asset",
    "ttf": "font", "otf": "font", "fontsettings": "font", "ttc": "font",
    "uxml": "ui", "uss": "ui", "tss": "ui",
    "mp4": "video", "mov": "video", "webm": "video", "avi": "video",
    "txt": "doc", "md": "doc", "pdf": "doc", "rtf": "doc", "html": "doc", "htm": "doc", "url": "doc",
    "json": "data", "xml": "data", "csv": "data", "bytes": "data", "yaml": "data", "yml": "data",
    "vfx": "vfx", "vfxoperator": "vfx", "vfxblock": "vfx",
}
# Some extensions are conventionally cased; look up case-insensitively but also try exact.
EXT_KIND_LOWER = {k.lower(): v for k, v in EXT_KIND.items()}

# Unity YAML class IDs -> readable names (first document in the file = the "main" object).
CLASS_NAMES = {
    1: "GameObject", 2: "Component", 4: "Transform", 20: "Camera", 21: "Material", 23: "MeshRenderer",
    25: "Renderer", 28: "Texture2D", 29: "OcclusionCullingSettings", 30: "GraphicsSettings", 33: "MeshFilter",
    41: "OcclusionPortal", 43: "Mesh", 45: "Skybox", 47: "QualitySettings", 48: "Shader", 49: "TextAsset",
    50: "Rigidbody2D", 54: "Rigidbody", 64: "MeshCollider", 65: "BoxCollider", 72: "ComputeShader",
    74: "AnimationClip", 81: "AudioListener", 82: "AudioSource", 83: "AudioClip", 84: "RenderTexture",
    86: "CustomRenderTexture", 89: "Cubemap", 90: "Avatar", 91: "AnimatorController", 93: "RuntimeAnimatorController",
    95: "Animator", 96: "TrailRenderer", 102: "TextMesh", 104: "RenderSettings", 108: "Light",
    114: "MonoBehaviour", 115: "MonoScript", 117: "Texture3D", 119: "Projector", 120: "LineRenderer",
    121: "Flare", 122: "Halo", 123: "LensFlare", 124: "FlareLayer", 128: "Font", 134: "PhysicMaterial",
    135: "SphereCollider", 136: "CapsuleCollider", 137: "SkinnedMeshRenderer", 142: "AssetBundle",
    150: "PreloadData", 152: "MovieTexture", 156: "TerrainData", 157: "LightmapSettings", 171: "SparseTexture",
    180: "AudioMixer", 181: "AudioMixerGroup", 182: "AudioMixerSnapshot", 183: "AudioMixerEffectController",
    187: "Texture2DArray", 188: "CubemapArray", 198: "ParticleSystem", 199: "ParticleSystemRenderer",
    200: "ShaderVariantCollection", 205: "LODGroup", 206: "BlendTree", 207: "Motion", 208: "NavMeshObstacle",
    210: "SortingGroup", 212: "SpriteRenderer", 213: "Sprite", 215: "ReflectionProbe", 218: "Terrain",
    221: "AnimatorOverrideController", 222: "CanvasRenderer", 223: "Canvas", 224: "RectTransform",
    225: "CanvasGroup", 226: "BillboardAsset", 227: "BillboardRenderer", 228: "SpeedTreeWindAsset",
    240: "AudioMixerController", 258: "LightProbes", 271: "LightProbeGroup", 290: "AssetBundleManifest",
    319: "AvatarMask", 320: "PlayableDirector", 328: "VideoPlayer", 329: "VideoClip", 331: "SpriteMask",
    687078895: "SpriteAtlas", 1001: "PrefabInstance", 1101: "AnimatorStateTransition", 1102: "AnimatorState",
    1107: "AnimatorStateMachine", 1109: "AnimatorTransition", 1111: "AnimatorTransitionBase", 1120: "LightingDataAsset",
    1953259897: "TilemapCollider2D", 1971053207: "Tilemap", 1839735485: "TilemapRenderer",
}


def kind_for_path(path: str, is_folder: bool) -> tuple[str, str]:
    """Return (ext, kind) for an asset path."""
    if is_folder:
        return "", "folder"
    name = PurePosixPath(path).name
    ext = name.rsplit(".", 1)[1] if "." in name else ""
    kind = EXT_KIND.get(ext) or EXT_KIND_LOWER.get(ext.lower()) or ("other" if ext else "other")
    return ext, kind


def is_builtin_guid(guid: str) -> bool:
    return guid == NULL_GUID or bool(BUILTIN_GUID_RE.match(guid))


@dataclass
class Entry:
    guid: str
    path: str = ""
    is_folder: bool = False
    has_asset: bool = False
    has_preview: bool = False
    asset_size: int = 0
    is_text: bool = False
    is_yaml: bool = False
    scan_truncated: bool = False
    main_class_id: int | None = None
    importer: str | None = None
    labels: list[str] = field(default_factory=list)
    refs: set[str] = field(default_factory=set)
    meta_size: int = 0

    @property
    def name(self) -> str:
        return PurePosixPath(self.path).name if self.path else ""

    @property
    def ext_kind(self) -> tuple[str, str]:
        return kind_for_path(self.path, self.is_folder)

    @property
    def main_class(self) -> str | None:
        if self.main_class_id is None:
            return None
        return CLASS_NAMES.get(self.main_class_id, f"Class{self.main_class_id}")

    def finish(self) -> "Entry":
        self.refs.discard(self.guid)
        self.refs = {r for r in self.refs if not is_builtin_guid(r)}
        return self

    def to_row(self) -> dict:
        ext, kind = self.ext_kind
        return {
            "guid": self.guid,
            "path": self.path,
            "name": self.name,
            "ext": ext,
            "kind": kind,
            "importer": self.importer,
            "main_class": self.main_class,
            "size": self.asset_size,
            "is_folder": self.is_folder,
            "has_preview": self.has_preview,
            "is_text": self.is_text,
            "scan_truncated": self.scan_truncated,
            "labels": self.labels,
            "refs": sorted(self.refs),
        }


def _normalize_member_name(name: str) -> list[str]:
    if name.startswith("./"):
        name = name[2:]
    return [p for p in name.split("/") if p]


def parse_meta(entry: Entry, text: str) -> None:
    m = IMPORTER_RE.search(text)
    entry.importer = m.group(1) if m else None
    if re.search(r"^folderAsset:\s*yes", text, re.M):
        entry.is_folder = True
    lm = LABELS_RE.search(text)
    if lm:
        entry.labels = [ln[2:].strip() for ln in lm.group(1).splitlines() if ln.startswith("- ")]
    for g in GUID_REF_RE.findall(text.encode("utf-8", "replace")):
        entry.refs.add(g.decode().lower())


def scan_asset(entry: Entry, f: BinaryIO, size: int, max_scan: int = MAX_SCAN_BYTES) -> None:
    """Decide whether the asset is text; if so, harvest guid references and the main class id."""
    entry.has_asset = True
    entry.asset_size = size
    if size == 0:
        return
    head = f.read(min(size, HEAD_BYTES))
    if head.startswith(b"%YAML"):
        entry.is_yaml = True
    else:
        if b"\x00" in head:
            return
        try:
            head.decode("utf-8")
        except UnicodeDecodeError:
            # Could still be text cut mid-multibyte-char; try a shorter prefix.
            try:
                head[:-4].decode("utf-8")
            except UnicodeDecodeError:
                return
    entry.is_text = True
    if entry.is_yaml:
        cm = CLASS_ID_RE.search(head)
        if cm:
            entry.main_class_id = int(cm.group(1))
    if size > max_scan:
        entry.scan_truncated = True
        data = head
    else:
        data = head + f.read()
    for g in GUID_REF_RE.findall(data):
        entry.refs.add(g.decode().lower())


def iter_package(path: str, bufsize: int = DEFAULT_BUFSIZE, max_scan: int = MAX_SCAN_BYTES) -> Iterator[Entry]:
    """Stream a .unitypackage once, yielding one Entry per guid directory."""
    cur: Entry | None = None
    with open_stream(path, bufsize) as tar:
        for m in tar:
            parts = _normalize_member_name(m.name)
            if not parts or not GUID_DIR_RE.match(parts[0]):
                continue
            guid = parts[0].lower()
            if cur is None or cur.guid != guid:
                if cur is not None:
                    yield cur.finish()
                cur = Entry(guid=guid)
            if len(parts) < 2 or not m.isfile():
                continue
            leaf = parts[1]
            if leaf == "pathname":
                raw = tar.extractfile(m).read().decode("utf-8", "replace")
                first = raw.replace("\r\n", "\n").split("\n", 1)[0].strip()
                cur.path = first.replace("\\", "/")
            elif leaf == "asset.meta":
                cur.meta_size = m.size
                parse_meta(cur, tar.extractfile(m).read().decode("utf-8", "replace"))
            elif leaf == "asset":
                scan_asset(cur, tar.extractfile(m), m.size, max_scan)
            elif leaf == "preview.png":
                cur.has_preview = True
    if cur is not None:
        yield cur.finish()


def iter_members(path: str, guids: Iterable[str], bufsize: int = DEFAULT_BUFSIZE) -> Iterator[tuple[str, str, tarfile.TarInfo, BinaryIO]]:
    """Stream a package and yield (guid, leaf, tarinfo, fileobj) for members of the wanted guid dirs.

    Members are grouped per guid inside the tar, so once every wanted guid has been seen and we hit
    an unwanted one, we stop early. The fileobj must be consumed before advancing the iterator.
    """
    wanted = {g.lower() for g in guids}
    remaining = set(wanted)
    with open_stream(path, bufsize) as tar:
        for m in tar:
            parts = _normalize_member_name(m.name)
            if not parts or not GUID_DIR_RE.match(parts[0]):
                continue
            guid = parts[0].lower()
            if guid not in wanted:
                if not remaining:
                    break
                continue
            remaining.discard(guid)
            if len(parts) < 2 or not m.isfile():
                continue
            yield guid, parts[1], m, tar.extractfile(m)
