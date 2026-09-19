//! End-to-end tests against synthetic `.unitypackage` files (no network share needed).

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use tempfile::TempDir;

use uai::config::Config;
use uai::db::Database;
use uai::deps::resolve_closure;
use uai::exporter::{build_plan, cache_add, export_to_dir, export_to_unitypackage, scan_project_guids};
use uai::indexer::{index_library, IndexOptions};
use uai::model::{ExportRequest, SearchQuery};
use uai::service::{LocalService, Service};
use uai::unitypackage::{read_entries, read_package_header, ScanOptions};

const G_PREFAB: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
const G_MAT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2";
const G_TEX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3";
const G_FBX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa4";
const G_SCRIPT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa5";
const G_FOLDER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6";
const G_OTHER_TEX: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1";
const G_MISSING: &str = "cccccccccccccccccccccccccccccccc";
const URP_LIT: &str = "933532a4fcc9baf4fa0491de14d08ed7";

fn meta(guid: &str, importer: &str, folder: bool) -> Vec<u8> {
    let mut s = format!("fileFormatVersion: 2\nguid: {guid}\n");
    if folder {
        s.push_str("folderAsset: yes\n");
    }
    s.push_str(&format!("{importer}:\n  externalObjects: {{}}\n  userData: \n"));
    s.into_bytes()
}

struct E {
    guid: &'static str,
    path: &'static str,
    asset: Option<Vec<u8>>,
    meta: Vec<u8>,
    preview: bool,
}

/// Build a package; with `header`, emulate the Asset Store gzip FEXTRA header.
fn make_package(path: &Path, entries: &[E], header: Option<serde_json::Value>) {
    let mut tar = tar::Builder::new(Vec::new());
    for e in entries {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_size(0);
        h.set_mode(0o755);
        tar.append_data(&mut h, e.guid, &[][..]).unwrap();
        let mut add = |name: &str, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_mtime(1_700_000_000);
            tar.append_data(&mut h, format!("{}/{}", e.guid, name), data).unwrap();
        };
        if let Some(a) = &e.asset {
            add("asset", a);
        }
        add("asset.meta", &e.meta);
        add("pathname", format!("{}\n00", e.path).as_bytes());
        if e.preview {
            add("preview.png", b"\x89PNG\r\n\x1a\nfakepng");
        }
    }
    let raw = tar.into_inner().unwrap();
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&raw).unwrap();
    let gz = gz.finish().unwrap();
    match header {
        None => fs::write(path, gz).unwrap(),
        Some(h) => {
            let payload = serde_json::to_vec(&h).unwrap();
            let mut sub = b"A$".to_vec();
            sub.extend_from_slice(&(payload.len() as u16).to_le_bytes());
            sub.extend_from_slice(&payload);
            let mut out = gz[..10].to_vec();
            out[3] |= 4;
            out.extend_from_slice(&(sub.len() as u16).to_le_bytes());
            out.extend_from_slice(&sub);
            out.extend_from_slice(&gz[10..]);
            fs::write(path, out).unwrap();
        }
    }
}

fn prefab_yaml() -> String {
    format!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &100\nGameObject:\n  m_Name: Chest\n--- !u!23 &200\nMeshRenderer:\n  m_Materials:\n  - {{fileID: 2100000, guid: {G_MAT}, type: 2}}\n--- !u!33 &300\nMeshFilter:\n  m_Mesh: {{fileID: 4300000, guid: {G_FBX}, type: 3}}\n--- !u!114 &400\nMonoBehaviour:\n  m_Script: {{fileID: 11500000, guid: {G_SCRIPT}, type: 3}}\n  someMissing: {{fileID: 1, guid: {G_MISSING}, type: 2}}\n  builtin: {{fileID: 10754, guid: 0000000000000000f000000000000000, type: 0}}\n"
    )
}

fn mat_yaml() -> String {
    format!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!21 &2100000\nMaterial:\n  m_Name: Chest\n  m_Shader: {{fileID: 4800000, guid: {URP_LIT}, type: 3}}\n  m_SavedProperties:\n    m_TexEnvs:\n    - _BaseMap:\n        m_Texture: {{fileID: 2800000, guid: {G_TEX}, type: 3}}\n    - _Detail:\n        m_Texture: {{fileID: 2800000, guid: {G_OTHER_TEX}, type: 3}}\n"
    )
}

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    lib: PathBuf,
    cfg: Config,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let lib = root.join("lib");
    fs::create_dir_all(lib.join("Pub A/3D ModelsProps")).unwrap();
    fs::create_dir_all(lib.join("Pub B/Textures")).unwrap();
    make_package(
        &lib.join("Pub A/3D ModelsProps/Chest Pack.unitypackage"),
        &[
            E {
                guid: G_FOLDER,
                path: "Assets/Chest",
                asset: None,
                meta: meta(G_FOLDER, "DefaultImporter", true),
                preview: false,
            },
            E {
                guid: G_PREFAB,
                path: "Assets/Chest/Prefabs/Chest.prefab",
                asset: Some(prefab_yaml().into_bytes()),
                meta: meta(G_PREFAB, "PrefabImporter", false),
                preview: true,
            },
            E {
                guid: G_MAT,
                path: "Assets/Chest/Materials/Chest.mat",
                asset: Some(mat_yaml().into_bytes()),
                meta: meta(G_MAT, "NativeFormatImporter", false),
                preview: false,
            },
            E {
                guid: G_TEX,
                path: "Assets/Chest/Textures/Chest_Albedo.png",
                asset: Some(b"\x89PNG\x00\x00binary".repeat(100)),
                meta: meta(G_TEX, "TextureImporter", false),
                preview: false,
            },
            E {
                guid: G_FBX,
                path: "Assets/Chest/Models/SM_Chest.fbx",
                asset: Some(b"Kaydara FBX Binary\x00\x00".repeat(50)),
                meta: meta(G_FBX, "ModelImporter", false),
                preview: false,
            },
            E {
                guid: G_SCRIPT,
                path: "Assets/Chest/Scripts/ChestOpener.cs",
                asset: Some(b"using UnityEngine;\npublic class ChestOpener : MonoBehaviour {}\n".to_vec()),
                meta: meta(G_SCRIPT, "MonoImporter", false),
                preview: false,
            },
        ],
        Some(
            serde_json::json!({"title": "Chest Pack", "version": "1.2.3", "unity_version": "2022.3.1f1", "id": "12345",
            "category": {"id": "1", "label": "3D Models/Props"}, "publisher": {"id": "9", "label": "Pub A"}}),
        ),
    );
    make_package(
        &lib.join("Pub B/Textures/Shared Textures.unitypackage"),
        &[E {
            guid: G_OTHER_TEX,
            path: "Assets/Shared/Detail.png",
            asset: Some(b"\x89PNGbinary".to_vec()),
            meta: meta(G_OTHER_TEX, "TextureImporter", false),
            preview: false,
        }],
        None,
    );
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let cfg = Config { library: lib.clone(), home, server: None };
    let mut db = Database::open_with_previews(&cfg.db_path(), &cfg.previews_path()).unwrap();
    let opts = IndexOptions { workers: 1, ..Default::default() };
    let mut log = |_: &str| {};
    let rep = index_library(&cfg, &mut db, &opts, &mut log, None).unwrap();
    assert_eq!(rep.indexed, 2, "{rep:?}");
    assert_eq!(rep.errors, 0);
    assert_eq!(rep.previews, 1);
    Fixture { _tmp: tmp, root, lib, cfg }
}

#[test]
fn reader_handles_fextra_header_and_refs() {
    let f = fixture();
    let p = f.lib.join("Pub A/3D ModelsProps/Chest Pack.unitypackage");
    let entries = read_entries(&p, ScanOptions::default()).unwrap();
    let by: HashMap<&str, _> = entries.iter().map(|e| (e.guid.as_str(), e)).collect();
    assert_eq!(entries.len(), 6);
    assert_eq!(by[G_PREFAB].path, "Assets/Chest/Prefabs/Chest.prefab");
    assert!(by[G_FOLDER].is_folder);
    assert!(by[G_PREFAB].has_preview);
    assert_eq!(by[G_PREFAB].main_class().as_deref(), Some("GameObject"));
    assert_eq!(by[G_MAT].main_class().as_deref(), Some("Material"));
    let refs: Vec<&str> = by[G_PREFAB].refs.iter().map(|s| s.as_str()).collect();
    assert_eq!(refs, vec![G_MAT, G_FBX, G_SCRIPT, G_MISSING]);
    assert!(!by[G_TEX].is_text);
    assert!(by[G_SCRIPT].is_text);
    assert_eq!(read_package_header(&p).unwrap()["title"], "Chest Pack");
}

#[test]
fn index_metadata_incremental_and_search() {
    let f = fixture();
    let svc = LocalService::open(&f.cfg).unwrap();
    let pkg = svc.find_package("Chest Pack").unwrap();
    assert_eq!(pkg.version.as_deref(), Some("1.2.3"));
    assert_eq!(pkg.category_label.as_deref(), Some("3D Models/Props"));
    assert_eq!(pkg.entry_count, 6);
    assert!(pkg.previews_indexed);
    // incremental: second run skips everything
    let mut db = Database::open_with_previews(&f.cfg.db_path(), &f.cfg.previews_path()).unwrap();
    let mut log = |_: &str| {};
    let again =
        index_library(&f.cfg, &mut db, &IndexOptions { workers: 1, ..Default::default() }, &mut log, None).unwrap();
    assert_eq!(again.indexed, 0);
    assert_eq!(again.skipped, 2);
    // previews are stored and served
    assert_eq!(svc.db.get_preview(G_PREFAB).unwrap().unwrap(), b"\x89PNG\r\n\x1a\nfakepng");
    assert!(svc.preview("Chest.prefab", None).is_ok());

    let q = |s: &str, kind: Option<&str>, publisher: Option<&str>| {
        svc.search(&SearchQuery {
            query: s.into(),
            kind: kind.map(Into::into),
            publisher: publisher.map(Into::into),
            ..Default::default()
        })
        .unwrap()
    };
    assert_eq!(q("chest", Some("prefab"), None).iter().map(|a| a.guid.as_str()).collect::<Vec<_>>(), vec![G_PREFAB]);
    assert!(q("sm chest", None, None).iter().any(|a| a.guid == G_FBX)); // camelCase / underscore split
    assert_eq!(q("albedo", None, Some("Pub A"))[0].guid, G_TEX);
    assert!(q("albedo", None, Some("Pub B")).is_empty());
}

#[test]
fn closure_cross_package_and_unresolved() {
    let f = fixture();
    let svc = LocalService::open(&f.cfg).unwrap();
    let root = svc.resolve(G_PREFAB, None).unwrap();
    let cl = resolve_closure(&svc.db, std::slice::from_ref(&root), true, None).unwrap();
    let mut got: Vec<&str> = cl.assets().map(|a| a.guid.as_str()).collect();
    got.sort();
    let mut want = vec![G_PREFAB, G_MAT, G_TEX, G_FBX, G_SCRIPT, G_OTHER_TEX];
    want.sort();
    assert_eq!(got, want);
    assert_eq!(cl.get(G_TEX).unwrap().depth, 2);
    assert_eq!(cl.get(G_TEX).unwrap().via.as_deref(), Some(G_MAT));
    assert_eq!(cl.packages().len(), 2);
    assert!(cl.unresolved.contains_key(G_MISSING));
    assert!(cl.unresolved[URP_LIT].label.as_deref().unwrap().contains("URP"));
    let cl2 = resolve_closure(&svc.db, &[root], false, None).unwrap();
    assert!(!cl2.contains(G_SCRIPT));
    assert!(cl2.skipped_scripts.contains_key(G_SCRIPT));
    assert_eq!(svc.db.referrers(G_MAT, None).unwrap().len(), 1);
    // service-level output includes edges for tree rendering
    let out = svc.deps(&["Chest.prefab".into()], None, true, None).unwrap();
    assert_eq!(out.roots, vec![G_PREFAB.to_string()]);
    assert_eq!(out.edges[G_PREFAB].len(), 4);
    assert_eq!(out.packages.len(), 2);
}

#[test]
fn export_to_project_with_folders_and_conflicts() {
    let f = fixture();
    let svc = LocalService::open(&f.cfg).unwrap();
    let proj = f.root.join("Proj");
    fs::create_dir_all(proj.join("Assets/Elsewhere")).unwrap();
    fs::create_dir_all(proj.join("ProjectSettings")).unwrap();
    // Pre-existing asset in the project with the *texture's* guid at another path -> conflict.
    fs::write(proj.join("Assets/Elsewhere/Old.png"), b"x").unwrap();
    fs::write(proj.join("Assets/Elsewhere/Old.png.meta"), meta(G_TEX, "TextureImporter", false)).unwrap();
    let root = svc.resolve(G_PREFAB, None).unwrap();
    let plan = build_plan(&svc.db, &[root], true, true, true).unwrap();
    assert!(plan.assets.iter().any(|a| a.is_folder));
    let mut log = |_: &str| {};
    let guids = scan_project_guids(&proj);
    let res = export_to_dir(&f.cfg, &svc.db, &plan, &proj, false, false, &guids, &mut log).unwrap();
    let written: Vec<&str> = res.written.iter().map(|w| w.path.as_str()).collect();
    assert!(written.contains(&"Assets/Chest/Prefabs/Chest.prefab"));
    assert!(written.contains(&"Assets/Shared/Detail.png")); // cross-package dep
    assert!(written.contains(&"Assets/Chest")); // folder meta
    assert!(proj.join("Assets/Chest.meta").is_file());
    assert!(proj.join("Assets/Chest/Prefabs/Chest.prefab.meta").is_file());
    assert_eq!(res.conflicts.iter().map(|c| c.guid.as_str()).collect::<Vec<_>>(), vec![G_TEX]);
    assert!(!proj.join("Assets/Chest/Textures/Chest_Albedo.png").exists());
    assert!(res.missing_in_package.is_empty());
    // Re-export: everything already there is skipped, nothing rewritten.
    let res2 =
        export_to_dir(&f.cfg, &svc.db, &plan, &proj, false, false, &scan_project_guids(&proj), &mut log).unwrap();
    assert!(res2.written.is_empty());
    assert!(!res2.skipped_existing.is_empty());
}

#[test]
fn export_unitypackage_roundtrip_and_cache() {
    let f = fixture();
    let svc = LocalService::open(&f.cfg).unwrap();
    let root = svc.resolve(G_MAT, None).unwrap();
    let plan = build_plan(&svc.db, &[root], true, true, true).unwrap();
    let out = f.root.join("slim.unitypackage");
    let mut log = |_: &str| {};
    let res = export_to_unitypackage(&f.cfg, &svc.db, &plan, &out, false, &mut log).unwrap();
    assert!(out.is_file());
    assert!(res.missing_in_package.is_empty());
    let names: Vec<String> = {
        let file = fs::File::open(&out).unwrap();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        ar.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned()).collect()
    };
    for n in [
        format!("{G_MAT}/asset"),
        format!("{G_MAT}/asset.meta"),
        format!("{G_MAT}/pathname"),
        format!("{G_OTHER_TEX}/asset"),
    ] {
        assert!(names.contains(&n), "{n} missing from {names:?}");
    }
    // The slim package is itself readable by the indexer's reader.
    let entries = read_entries(&out, ScanOptions::default()).unwrap();
    assert!(entries.iter().any(|e| e.guid == G_MAT && e.path == "Assets/Chest/Materials/Chest.mat"));
    // Now cache a package and export again from the cache.
    let pkg = svc.find_package("Chest Pack").unwrap();
    cache_add(&f.cfg, &pkg, &mut log).unwrap();
    assert!(uai::exporter::cache_complete(&f.cfg, pkg.id));
    let out2 = f.root.join("slim2.unitypackage");
    let res2 = export_to_unitypackage(&f.cfg, &svc.db, &plan, &out2, false, &mut log).unwrap();
    let a: std::collections::BTreeSet<_> = res.written.iter().map(|w| w.guid.clone()).collect();
    let b: std::collections::BTreeSet<_> = res2.written.iter().map(|w| w.guid.clone()).collect();
    assert_eq!(a, b);
    // Text extraction works from the cache too.
    let t = svc.text("Chest.mat", None, 1_000_000).unwrap();
    assert!(t.text.contains("m_Shader"));
}

#[test]
fn cli_json_roundtrip() {
    let f = fixture();
    let bin = env!("CARGO_BIN_EXE_uai");
    let run = |args: &[&str]| {
        let out = std::process::Command::new(bin)
            .args(args)
            .env("UAI_HOME", &f.cfg.home)
            .env("UAI_LIBRARY", &f.lib)
            .env_remove("UAI_SERVER")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    let data: serde_json::Value = serde_json::from_str(&run(&["search", "chest", "--json"])).unwrap();
    assert!(data.as_array().unwrap().iter().any(|d| d["guid"] == G_PREFAB));
    let deps: serde_json::Value = serde_json::from_str(&run(&["deps", "Chest.prefab", "--json"])).unwrap();
    assert_eq!(deps["roots"], serde_json::json!([G_PREFAB]));
    let out_dir = f.root.join("cli_out");
    let payload: serde_json::Value = serde_json::from_str(&run(&[
        "export",
        "Chest.prefab",
        "--out",
        out_dir.to_str().unwrap(),
        "--json",
        "--no-scripts",
    ]))
    .unwrap();
    assert_eq!(payload["skipped_scripts"], serde_json::json!(["Assets/Chest/Scripts/ChestOpener.cs"]));
    assert!(out_dir.join("Assets/Chest/Prefabs/Chest.prefab").is_file());
    // ambiguity / not-found are clean errors, not panics
    let out = std::process::Command::new(bin)
        .args(["info", "nope.prefab"])
        .env("UAI_HOME", &f.cfg.home)
        .env("UAI_LIBRARY", &f.lib)
        .env_remove("UAI_SERVER")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no asset matches"));
}

#[test]
fn server_and_remote_client() {
    let f = fixture();
    // Spin up the HTTP server on an ephemeral port in a background thread.
    let cfg = f.cfg.clone();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    let app = uai::server::router(cfg);
    let handle = rt.spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");

    let remote = uai::client::RemoteService::new(&base).unwrap();
    assert_eq!(remote.stats().unwrap().packages, 2);
    assert_eq!(remote.packages().unwrap().len(), 2);
    let hits = remote
        .search(&SearchQuery { query: "chest".into(), kind: Some("prefab".into()), ..Default::default() })
        .unwrap();
    assert_eq!(hits.len(), 1);
    let info = remote.info("Chest.prefab", None).unwrap();
    assert_eq!(info.asset.guid, G_PREFAB);
    assert_eq!(info.refs.len(), 4);
    let cl = remote.deps(&["Chest.prefab".into()], None, true, None).unwrap();
    assert_eq!(cl.assets.len(), 6);
    assert_eq!(remote.preview("Chest.prefab", None).unwrap(), b"\x89PNG\r\n\x1a\nfakepng");
    assert!(remote.text("Chest.mat", None, 100_000).unwrap().text.contains("m_Shader"));
    // Error mapping: ambiguous -> UaiError::Ambiguous with candidates; missing -> NotFound.
    let err = remote.resolve("doesnotexist.png", None).unwrap_err();
    assert!(matches!(err.downcast_ref::<uai::error::UaiError>(), Some(uai::error::UaiError::NotFound(_))));
    // Remote export into a directory: plan from the server, package streamed, unpacked locally.
    let out = f.root.join("remote_out");
    let req = ExportRequest {
        identifiers: vec!["Chest.prefab".into()],
        package: None,
        include_deps: true,
        include_scripts: false,
        include_folders: true,
    };
    let mut log = |_: &str| {};
    let res = remote
        .export(
            &req,
            &uai::service::ExportDest::Dir(out.clone()),
            &uai::service::ExportOptions { force: false, dry_run: false, conflict_check: false },
            &mut log,
        )
        .unwrap();
    assert!(res.result.missing_in_package.is_empty(), "{:?}", res.result.missing_in_package);
    assert!(out.join("Assets/Chest/Prefabs/Chest.prefab").is_file());
    assert!(out.join("Assets/Shared/Detail.png.meta").is_file());
    assert!(!out.join("Assets/Chest/Scripts/ChestOpener.cs").exists());
    assert_eq!(res.plan.skipped_scripts, vec!["Assets/Chest/Scripts/ChestOpener.cs".to_string()]);
    // And as a .unitypackage file.
    let pkg_out = f.root.join("remote.unitypackage");
    remote
        .export(&req, &uai::service::ExportDest::UnityPackage(pkg_out.clone()), &Default::default(), &mut log)
        .unwrap();
    let mut header = [0u8; 2];
    fs::File::open(&pkg_out).unwrap().read_exact(&mut header).unwrap();
    assert_eq!(header, [0x1f, 0x8b]);
    handle.abort();
}
