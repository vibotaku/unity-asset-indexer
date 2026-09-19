//! Streaming reader for `.unitypackage` files.
//!
//! A `.unitypackage` is a gzipped tar whose top-level directories are asset GUIDs:
//!
//! ```text
//! <guid>/asset         the asset bytes (absent for folders)
//! <guid>/asset.meta    Unity .meta YAML (importer settings; contains the guid)
//! <guid>/pathname      "Assets/Path/To/File.ext" (+ an optional second line)
//! <guid>/preview.png   optional thumbnail
//! ```
//!
//! Dependencies between assets are `guid: <32 hex>` references inside text (YAML/JSON) assets and
//! inside `.meta` files. Indexing streams every package exactly once and records metadata + references
//! (+ the preview thumbnails); binaries are never extracted.
//!
//! Asset Store packages carry a gzip FEXTRA header with JSON metadata (title, version, publisher...).
//! flate2's `GzDecoder` handles that header correctly; [`read_package_header`] parses it.

use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use regex::bytes::Regex as BytesRegex;
use regex::Regex;
use serde::{Deserialize, Serialize};

pub const DEFAULT_BUFSIZE: usize = 4 * 1024 * 1024;
pub const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
pub const HEAD_BYTES: usize = 8192;
pub const NULL_GUID: &str = "00000000000000000000000000000000";

fn guid_ref_re() -> &'static BytesRegex {
    static RE: OnceLock<BytesRegex> = OnceLock::new();
    // Matches YAML `guid: abc...`, JSON `"guid": "abc..."` and escaped-JSON `\"guid\":\"abc...\"`.
    RE.get_or_init(|| BytesRegex::new(r#"guid[\\"]*\s*:\s*[\\"]*([0-9a-fA-F]{32})"#).unwrap())
}
fn class_id_re() -> &'static BytesRegex {
    static RE: OnceLock<BytesRegex> = OnceLock::new();
    RE.get_or_init(|| BytesRegex::new(r"(?m)^--- !u!(\d+)").unwrap())
}
fn importer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^([A-Za-z0-9]+Importer):").unwrap())
}
fn folder_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^folderAsset:\s*yes").unwrap())
}
fn labels_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^labels:\n((?:- .*\n?)+)").unwrap())
}

pub fn is_guid(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `0000...0000`, `...e000...` (built-in resources) and `...f000...` (built-in extra) guids.
pub fn is_builtin_guid(guid: &str) -> bool {
    if guid == NULL_GUID {
        return true;
    }
    let b = guid.as_bytes();
    b.len() == 32
        && b[..16].iter().all(|&c| c == b'0')
        && b[16].is_ascii_hexdigit()
        && b[17..].iter().all(|&c| c == b'0')
}

// ----- gzip header ------------------------------------------------------------------------------------

/// Asset Store packages embed JSON in the gzip extra field (store id, unity version, title, ...).
pub fn read_package_header(path: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut f = File::open(path).ok()?;
    let mut head = vec![0u8; 65536];
    let mut n = 0;
    while n < head.len() {
        match f.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    head.truncate(n);
    parse_gzip_extra_json(&head)
}

pub fn parse_gzip_extra_json(head: &[u8]) -> Option<serde_json::Map<String, serde_json::Value>> {
    if head.len() < 12 || head[0] != 0x1f || head[1] != 0x8b || head[3] & 4 == 0 {
        return None;
    }
    let xlen = u16::from_le_bytes([head[10], head[11]]) as usize;
    let extra = head.get(12..(12 + xlen).min(head.len()))?;
    let mut pos = 0;
    while pos + 4 <= extra.len() {
        let ln = u16::from_le_bytes([extra[pos + 2], extra[pos + 3]]) as usize;
        let end = (pos + 4 + ln).min(extra.len());
        let data = &extra[pos + 4..end];
        pos += 4 + ln;
        if let Ok(serde_json::Value::Object(m)) = serde_json::from_slice::<serde_json::Value>(data) {
            return Some(m);
        }
        if let Ok(serde_json::Value::Object(m)) =
            serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(data))
        {
            return Some(m);
        }
    }
    None
}

// ----- kinds ------------------------------------------------------------------------------------------

/// Extension -> coarse kind used for filtering/search.
const EXT_KIND: &[(&str, &str)] = &[
    ("prefab", "prefab"),
    ("mat", "material"),
    ("shader", "shader"),
    ("shadergraph", "shader"),
    ("shadersubgraph", "shader"),
    ("hlsl", "shader"),
    ("cginc", "shader"),
    ("cg", "shader"),
    ("compute", "shader"),
    ("raytrace", "shader"),
    ("shadervariants", "shader"),
    ("png", "texture"),
    ("jpg", "texture"),
    ("jpeg", "texture"),
    ("tga", "texture"),
    ("psd", "texture"),
    ("psb", "texture"),
    ("tif", "texture"),
    ("tiff", "texture"),
    ("exr", "texture"),
    ("hdr", "texture"),
    ("bmp", "texture"),
    ("gif", "texture"),
    ("dds", "texture"),
    ("webp", "texture"),
    ("rendertexture", "texture"),
    ("cubemap", "texture"),
    ("fbx", "model"),
    ("obj", "model"),
    ("blend", "model"),
    ("dae", "model"),
    ("3ds", "model"),
    ("max", "model"),
    ("ma", "model"),
    ("mb", "model"),
    ("c4d", "model"),
    ("glb", "model"),
    ("gltf", "model"),
    ("ply", "model"),
    ("stl", "model"),
    ("wav", "audio"),
    ("mp3", "audio"),
    ("ogg", "audio"),
    ("aif", "audio"),
    ("aiff", "audio"),
    ("flac", "audio"),
    ("mod", "audio"),
    ("it", "audio"),
    ("s3m", "audio"),
    ("xm", "audio"),
    ("mixer", "audio"),
    ("anim", "animation"),
    ("controller", "animation"),
    ("overridecontroller", "animation"),
    ("mask", "animation"),
    ("playable", "animation"),
    ("signal", "animation"),
    ("unity", "scene"),
    ("cs", "script"),
    ("js", "script"),
    ("boo", "script"),
    ("dll", "script"),
    ("asmdef", "script"),
    ("asmref", "script"),
    ("pdb", "script"),
    ("mdb", "script"),
    ("so", "script"),
    ("dylib", "script"),
    ("bundle", "script"),
    ("aar", "script"),
    ("jar", "script"),
    ("a", "script"),
    ("mm", "script"),
    ("m", "script"),
    ("h", "script"),
    ("cpp", "script"),
    ("c", "script"),
    ("asset", "asset"),
    ("preset", "asset"),
    ("physicmaterial", "asset"),
    ("physicsmaterial2d", "asset"),
    ("terrainlayer", "asset"),
    ("brush", "asset"),
    ("flare", "asset"),
    ("guiskin", "asset"),
    ("lighting", "asset"),
    ("giparams", "asset"),
    ("spriteatlas", "asset"),
    ("spriteatlasv2", "asset"),
    ("inputactions", "asset"),
    ("ttf", "font"),
    ("otf", "font"),
    ("fontsettings", "font"),
    ("ttc", "font"),
    ("uxml", "ui"),
    ("uss", "ui"),
    ("tss", "ui"),
    ("mp4", "video"),
    ("mov", "video"),
    ("webm", "video"),
    ("avi", "video"),
    ("txt", "doc"),
    ("md", "doc"),
    ("pdf", "doc"),
    ("rtf", "doc"),
    ("html", "doc"),
    ("htm", "doc"),
    ("url", "doc"),
    ("json", "data"),
    ("xml", "data"),
    ("csv", "data"),
    ("bytes", "data"),
    ("yaml", "data"),
    ("yml", "data"),
    ("vfx", "vfx"),
    ("vfxoperator", "vfx"),
    ("vfxblock", "vfx"),
];

pub const KINDS: &[&str] = &[
    "prefab",
    "material",
    "shader",
    "texture",
    "model",
    "audio",
    "animation",
    "scene",
    "script",
    "asset",
    "font",
    "ui",
    "video",
    "doc",
    "data",
    "vfx",
    "folder",
    "other",
];

/// Return (ext, kind) for an asset path. The extension keeps its original casing.
pub fn kind_for_path(path: &str, is_folder: bool) -> (String, &'static str) {
    if is_folder {
        return (String::new(), "folder");
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = match name.rfind('.') {
        Some(i) => &name[i + 1..],
        None => "",
    };
    let lower = ext.to_ascii_lowercase();
    let kind = EXT_KIND.iter().find(|(e, _)| *e == lower).map(|(_, k)| *k).unwrap_or("other");
    (ext.to_string(), kind)
}

/// Unity YAML class IDs -> readable names (first document in the file = the "main" object).
pub fn class_name(id: u64) -> String {
    let name = match id {
        1 => "GameObject",
        2 => "Component",
        4 => "Transform",
        20 => "Camera",
        21 => "Material",
        23 => "MeshRenderer",
        25 => "Renderer",
        28 => "Texture2D",
        29 => "OcclusionCullingSettings",
        30 => "GraphicsSettings",
        33 => "MeshFilter",
        41 => "OcclusionPortal",
        43 => "Mesh",
        45 => "Skybox",
        47 => "QualitySettings",
        48 => "Shader",
        49 => "TextAsset",
        50 => "Rigidbody2D",
        54 => "Rigidbody",
        64 => "MeshCollider",
        65 => "BoxCollider",
        72 => "ComputeShader",
        74 => "AnimationClip",
        81 => "AudioListener",
        82 => "AudioSource",
        83 => "AudioClip",
        84 => "RenderTexture",
        86 => "CustomRenderTexture",
        89 => "Cubemap",
        90 => "Avatar",
        91 => "AnimatorController",
        93 => "RuntimeAnimatorController",
        95 => "Animator",
        96 => "TrailRenderer",
        102 => "TextMesh",
        104 => "RenderSettings",
        108 => "Light",
        114 => "MonoBehaviour",
        115 => "MonoScript",
        117 => "Texture3D",
        119 => "Projector",
        120 => "LineRenderer",
        121 => "Flare",
        122 => "Halo",
        123 => "LensFlare",
        124 => "FlareLayer",
        128 => "Font",
        134 => "PhysicMaterial",
        135 => "SphereCollider",
        136 => "CapsuleCollider",
        137 => "SkinnedMeshRenderer",
        142 => "AssetBundle",
        150 => "PreloadData",
        152 => "MovieTexture",
        156 => "TerrainData",
        157 => "LightmapSettings",
        171 => "SparseTexture",
        180 => "AudioMixer",
        181 => "AudioMixerGroup",
        182 => "AudioMixerSnapshot",
        183 => "AudioMixerEffectController",
        187 => "Texture2DArray",
        188 => "CubemapArray",
        198 => "ParticleSystem",
        199 => "ParticleSystemRenderer",
        200 => "ShaderVariantCollection",
        205 => "LODGroup",
        206 => "BlendTree",
        207 => "Motion",
        208 => "NavMeshObstacle",
        210 => "SortingGroup",
        212 => "SpriteRenderer",
        213 => "Sprite",
        215 => "ReflectionProbe",
        218 => "Terrain",
        221 => "AnimatorOverrideController",
        222 => "CanvasRenderer",
        223 => "Canvas",
        224 => "RectTransform",
        225 => "CanvasGroup",
        226 => "BillboardAsset",
        227 => "BillboardRenderer",
        228 => "SpeedTreeWindAsset",
        240 => "AudioMixerController",
        258 => "LightProbes",
        271 => "LightProbeGroup",
        290 => "AssetBundleManifest",
        319 => "AvatarMask",
        320 => "PlayableDirector",
        328 => "VideoPlayer",
        329 => "VideoClip",
        331 => "SpriteMask",
        687078895 => "SpriteAtlas",
        1001 => "PrefabInstance",
        1101 => "AnimatorStateTransition",
        1102 => "AnimatorState",
        1107 => "AnimatorStateMachine",
        1109 => "AnimatorTransition",
        1111 => "AnimatorTransitionBase",
        1120 => "LightingDataAsset",
        1953259897 => "TilemapCollider2D",
        1971053207 => "Tilemap",
        1839735485 => "TilemapRenderer",
        _ => return format!("Class{id}"),
    };
    name.to_string()
}

// ----- entries ----------------------------------------------------------------------------------------

/// One guid directory of a package, as harvested while streaming.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    pub guid: String,
    pub path: String,
    pub is_folder: bool,
    pub has_asset: bool,
    pub has_preview: bool,
    pub asset_size: u64,
    pub is_text: bool,
    pub is_yaml: bool,
    pub scan_truncated: bool,
    pub main_class_id: Option<u64>,
    pub importer: Option<String>,
    pub labels: Vec<String>,
    pub refs: BTreeSet<String>,
    pub meta_size: u64,
    /// The `preview.png` bytes, when the reader was asked to keep them.
    #[serde(skip)]
    pub preview: Option<Vec<u8>>,
}

impl Entry {
    pub fn new(guid: &str) -> Self {
        Entry { guid: guid.to_ascii_lowercase(), ..Default::default() }
    }
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }
    pub fn ext_kind(&self) -> (String, &'static str) {
        kind_for_path(&self.path, self.is_folder)
    }
    pub fn main_class(&self) -> Option<String> {
        self.main_class_id.map(class_name)
    }
    fn finish(mut self) -> Self {
        let own = self.guid.clone();
        self.refs.retain(|r| r != &own && !is_builtin_guid(r));
        self
    }
}

pub fn parse_meta(entry: &mut Entry, text: &str) {
    entry.importer = importer_re().captures(text).map(|c| c[1].to_string());
    if folder_re().is_match(text) {
        entry.is_folder = true;
    }
    if let Some(m) = labels_re().captures(text) {
        entry.labels = m[1]
            .lines()
            .filter_map(|l| l.strip_prefix("- ").map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    collect_guid_refs(text.as_bytes(), &mut entry.refs);
}

pub fn collect_guid_refs(data: &[u8], out: &mut BTreeSet<String>) {
    for c in guid_ref_re().captures_iter(data) {
        let g = std::str::from_utf8(&c[1]).unwrap_or_default().to_ascii_lowercase();
        out.insert(g);
    }
}

fn looks_like_text(head: &[u8]) -> bool {
    if head.contains(&0u8) {
        return false;
    }
    if std::str::from_utf8(head).is_ok() {
        return true;
    }
    // Could still be text cut mid-multibyte-char; try a shorter prefix.
    head.len() > 4 && std::str::from_utf8(&head[..head.len() - 4]).is_ok()
}

/// Decide whether the asset is text; if so, harvest guid references and the main class id.
pub fn scan_asset<R: Read>(entry: &mut Entry, f: &mut R, size: u64, max_scan: u64) -> io::Result<()> {
    entry.has_asset = true;
    entry.asset_size = size;
    if size == 0 {
        return Ok(());
    }
    let want = (size as usize).min(HEAD_BYTES);
    let mut head = vec![0u8; want];
    let n = read_fully(f, &mut head)?;
    head.truncate(n);
    if head.starts_with(b"%YAML") {
        entry.is_yaml = true;
    } else if !looks_like_text(&head) {
        return Ok(());
    }
    entry.is_text = true;
    if entry.is_yaml {
        if let Some(c) = class_id_re().captures(&head) {
            entry.main_class_id = std::str::from_utf8(&c[1]).ok().and_then(|s| s.parse().ok());
        }
    }
    if size > max_scan {
        entry.scan_truncated = true;
        collect_guid_refs(&head, &mut entry.refs);
    } else {
        let mut data = head;
        data.reserve((size as usize).saturating_sub(data.len()));
        f.read_to_end(&mut data)?;
        collect_guid_refs(&data, &mut entry.refs);
    }
    Ok(())
}

fn read_fully<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

// ----- streaming --------------------------------------------------------------------------------------

/// Wraps a reader so every read adds to a shared counter (indexing progress).
pub struct CountingReader<R> {
    inner: R,
    counter: Arc<AtomicU64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.counter.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

pub type PackageStream = tar::Archive<GzDecoder<Box<dyn Read + Send>>>;

/// Open a `.unitypackage` for sequential reading. `counter` (optional) receives compressed bytes read.
pub fn open_stream(path: &Path, counter: Option<Arc<AtomicU64>>) -> Result<PackageStream> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let buffered = BufReader::with_capacity(DEFAULT_BUFSIZE, file);
    let src: Box<dyn Read + Send> = match counter {
        Some(c) => Box::new(CountingReader { inner: buffered, counter: c }),
        None => Box::new(buffered),
    };
    let gz = GzDecoder::new(src);
    let mut archive = tar::Archive::new(gz);
    archive.set_ignore_zeros(true);
    archive.set_unpack_xattrs(false);
    archive.set_preserve_permissions(false);
    Ok(archive)
}

/// `./<guid>/asset` -> Some(("<guid>", Some("asset"))); top-level junk -> None.
pub fn split_member_name(name: &str) -> Option<(String, Option<String>)> {
    let name = name.strip_prefix("./").unwrap_or(name);
    let mut parts = name.split('/').filter(|p| !p.is_empty());
    let first = parts.next()?;
    if !is_guid(first) {
        return None;
    }
    Some((first.to_ascii_lowercase(), parts.next().map(|s| s.to_string())))
}

/// Options for [`scan_package`].
#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    pub max_scan: u64,
    pub keep_previews: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions { max_scan: MAX_SCAN_BYTES, keep_previews: false }
    }
}

/// Stream a package once, calling `on_entry` for every guid directory (in tar order).
pub fn scan_package<F>(path: &Path, opts: ScanOptions, counter: Option<Arc<AtomicU64>>, mut on_entry: F) -> Result<()>
where
    F: FnMut(Entry) -> Result<()>,
{
    let mut archive = open_stream(path, counter)?;
    let mut cur: Option<Entry> = None;
    for entry in archive.entries().context("reading tar")? {
        let mut entry = entry.context("reading tar entry")?;
        let name = match entry.path() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => continue,
        };
        let Some((guid, leaf)) = split_member_name(&name) else { continue };
        if cur.as_ref().map(|c| c.guid != guid).unwrap_or(true) {
            if let Some(done) = cur.take() {
                on_entry(done.finish())?;
            }
            cur = Some(Entry::new(&guid));
        }
        let c = cur.as_mut().unwrap();
        let Some(leaf) = leaf else { continue };
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let size = entry.header().size().unwrap_or(0);
        match leaf.as_str() {
            "pathname" => {
                let mut raw = String::new();
                entry.read_to_string(&mut raw).ok();
                let first = raw.replace("\r\n", "\n");
                let first = first.split('\n').next().unwrap_or("").trim();
                c.path = first.replace('\\', "/");
            }
            "asset.meta" => {
                c.meta_size = size;
                let mut buf = Vec::with_capacity(size as usize);
                entry.read_to_end(&mut buf)?;
                let text = String::from_utf8_lossy(&buf);
                parse_meta(c, &text);
            }
            "asset" => {
                scan_asset(c, &mut entry, size, opts.max_scan)?;
            }
            "preview.png" => {
                c.has_preview = true;
                if opts.keep_previews {
                    let mut buf = Vec::with_capacity(size as usize);
                    entry.read_to_end(&mut buf)?;
                    c.preview = Some(buf);
                }
            }
            _ => {}
        }
    }
    if let Some(done) = cur.take() {
        on_entry(done.finish())?;
    }
    Ok(())
}

/// Convenience: collect every entry of a package.
pub fn read_entries(path: &Path, opts: ScanOptions) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    scan_package(path, opts, None, |e| {
        out.push(e);
        Ok(())
    })?;
    Ok(out)
}

/// A member of a wanted guid directory, handed to the callback of [`for_each_member`].
pub struct Member<'a> {
    pub guid: String,
    pub leaf: String,
    pub size: u64,
    pub mtime: u64,
    pub reader: &'a mut dyn Read,
}

/// Stream a package and call `f` for members of the wanted guid dirs (`asset`, `asset.meta`, `pathname`,
/// `preview.png`). Members are grouped per guid inside the tar, so once every wanted guid has been seen
/// and an unwanted one shows up, reading stops early.
pub fn for_each_member<F>(path: &Path, guids: &HashSet<String>, mut f: F) -> Result<()>
where
    F: FnMut(Member<'_>) -> Result<()>,
{
    let wanted: HashSet<String> = guids.iter().map(|g| g.to_ascii_lowercase()).collect();
    let mut remaining = wanted.clone();
    let mut archive = open_stream(path, None)?;
    for entry in archive.entries().context("reading tar")? {
        let mut entry = entry.context("reading tar entry")?;
        let name = match entry.path() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => continue,
        };
        let Some((guid, leaf)) = split_member_name(&name) else { continue };
        if !wanted.contains(&guid) {
            if remaining.is_empty() {
                break;
            }
            continue;
        }
        remaining.remove(&guid);
        let Some(leaf) = leaf else { continue };
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let size = entry.header().size().unwrap_or(0);
        let mtime = entry.header().mtime().unwrap_or(0);
        f(Member { guid, leaf, size, mtime, reader: &mut entry })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_for_path("Assets/A/B.prefab", false), ("prefab".into(), "prefab"));
        assert_eq!(kind_for_path("Assets/A/B.FBX", false), ("FBX".into(), "model"));
        assert_eq!(kind_for_path("Assets/A/README", false), ("".into(), "other"));
        assert_eq!(kind_for_path("Assets/A", true), ("".into(), "folder"));
    }

    #[test]
    fn builtin() {
        assert!(is_builtin_guid(NULL_GUID));
        assert!(is_builtin_guid("0000000000000000f000000000000000"));
        assert!(is_builtin_guid("0000000000000000e000000000000000"));
        assert!(!is_builtin_guid("933532a4fcc9baf4fa0491de14d08ed7"));
    }

    #[test]
    fn refs() {
        let mut s = BTreeSet::new();
        collect_guid_refs(br#"m_Shader: {fileID: 4800000, guid: 933532A4fcc9baf4fa0491de14d08ed7, type: 3}"#, &mut s);
        collect_guid_refs(
            br#"{"guid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1"} \"guid\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2\""#,
            &mut s,
        );
        assert_eq!(s.len(), 3);
        assert!(s.contains("933532a4fcc9baf4fa0491de14d08ed7"));
    }

    #[test]
    fn member_names() {
        assert_eq!(
            split_member_name("./aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1/asset"),
            Some(("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1".into(), Some("asset".into())))
        );
        assert_eq!(
            split_member_name("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1/"),
            Some(("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1".into(), None))
        );
        assert_eq!(split_member_name("junk/asset"), None);
    }
}
