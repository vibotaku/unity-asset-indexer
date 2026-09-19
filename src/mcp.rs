//! MCP (Model Context Protocol) server over stdio, for agents such as Claude Code and Codex.
//!
//! The protocol is JSON-RPC 2.0, one message per line. Only the parts needed for a tool server are
//! implemented (`initialize`, `ping`, `tools/list`, `tools/call`), which keeps the binary free of a
//! large SDK dependency. Works in local and remote (`--server`) mode alike.

use std::io::{BufRead, Write};

use anyhow::Result;
use serde_json::{json, Value};

use crate::config::Config;
use crate::model::*;
use crate::service::{open_service, ExportDest, ExportOptions, Service};

const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Search a library of Unity .unitypackage files by asset name, inspect an asset's dependency closure \
(prefab -> mesh/material/texture/shader/script, across packages), and export exactly those files into a Unity project's \
Assets/ folder with their .meta files so guids stay intact. Typical flow: search_assets -> dependencies (optional) -> \
export_assets(project_dir=...). Identifiers accept a guid, 'Package::Assets/path', an Assets/ path, a path suffix or a \
bare file name.";

fn tools() -> Value {
    let ident = "guid | Package::Assets/path | Assets/path | path suffix | file name";
    json!([
        {
            "name": "list_packages",
            "description": "List every indexed .unitypackage (id, name, publisher, category, version, asset count, sizes).",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": "search_assets",
            "description": "Full-text search over asset paths in all packages. Words are prefix-matched and camelCase/underscores are split ('sm env lily' finds SM_Env_Lily_01). Returns guid, path, kind, size (bytes), package, has_preview, is_text.",
            "inputSchema": {"type": "object", "required": ["query"], "properties": {
                "query": {"type": "string"},
                "kind": {"type": "string", "description": "comma list of: prefab, material, shader, texture, model, audio, animation, scene, script, asset, font, ui, vfx"},
                "package": {"type": "string", "description": "substring filter on package name"},
                "publisher": {"type": "string", "description": "substring filter on publisher"},
                "ext": {"type": "string", "description": "file extension, e.g. fbx"},
                "limit": {"type": "integer", "default": 30}
            }}
        },
        {
            "name": "list_package_assets",
            "description": "List assets inside one package (by id or partial name), optionally under an Assets/... prefix or of one kind.",
            "inputSchema": {"type": "object", "required": ["package"], "properties": {
                "package": {"type": "string"},
                "path_prefix": {"type": "string"},
                "kind": {"type": "string"},
                "limit": {"type": "integer", "default": 500}
            }}
        },
        {
            "name": "asset_info",
            "description": "Details for one asset: metadata, direct dependencies (resolved), and assets that use it.",
            "inputSchema": {"type": "object", "required": ["identifier"], "properties": {
                "identifier": {"type": "string", "description": ident},
                "package": {"type": "string"}
            }}
        },
        {
            "name": "dependencies",
            "description": "Transitive dependency closure for one or more assets, across packages: every asset that would be needed (with depth and the referrer that pulled it in), unresolved guids (Unity built-ins, UPM packages, or packages not in the library), and total bytes.",
            "inputSchema": {"type": "object", "required": ["identifiers"], "properties": {
                "identifiers": {"type": "array", "items": {"type": "string"}, "description": ident},
                "include_scripts": {"type": "boolean", "default": true},
                "max_depth": {"type": "integer"}
            }}
        },
        {
            "name": "export_assets",
            "description": "Extract assets (+ transitive dependencies) with their .meta files. Exactly one destination: project_dir (a Unity project; files land under its Assets/ at their original paths and guid conflicts with existing project assets are detected), out_dir (plain folder), or unitypackage_path (a slim .unitypackage). Existing files are skipped unless force=true. Use dry_run=true to preview.",
            "inputSchema": {"type": "object", "required": ["identifiers"], "properties": {
                "identifiers": {"type": "array", "items": {"type": "string"}, "description": ident},
                "project_dir": {"type": "string"},
                "out_dir": {"type": "string"},
                "unitypackage_path": {"type": "string"},
                "include_deps": {"type": "boolean", "default": true},
                "include_scripts": {"type": "boolean", "default": true},
                "force": {"type": "boolean", "default": false},
                "dry_run": {"type": "boolean", "default": false}
            }}
        },
        {
            "name": "read_text_asset",
            "description": "Return the text of a YAML/script/shader asset (prefab structure, material properties, C# source...).",
            "inputSchema": {"type": "object", "required": ["identifier"], "properties": {
                "identifier": {"type": "string", "description": ident},
                "max_bytes": {"type": "integer", "default": 200000}
            }}
        }
    ])
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string()).filter(|x| !x.trim().is_empty())
}
fn b(v: &Value, k: &str, default: bool) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(default)
}
fn n(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(|x| x.as_u64())
}
fn strings(v: &Value, k: &str) -> Vec<String> {
    match v.get(k) {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn slim_asset(a: &Asset) -> Value {
    json!({
        "guid": a.guid, "path": a.path, "name": a.name, "kind": a.kind, "ext": a.ext, "main_class": a.main_class,
        "size": a.size, "package": a.package, "package_id": a.package_id, "publisher": a.publisher,
        "has_preview": a.has_preview, "is_text": a.is_text
    })
}

fn call_tool(svc: &dyn Service, name: &str, args: &Value) -> Result<Value> {
    Ok(match name {
        "list_packages" => {
            let pk = svc.packages()?;
            Value::Array(
                pk.iter()
                    .map(|r| {
                        json!({
                            "id": r.id, "name": r.name, "title": r.title, "publisher": r.publisher, "category_label": r.display_category(),
                            "version": r.version, "unity_version": r.unity_version, "pubdate": r.pubdate, "entry_count": r.entry_count,
                            "total_bytes": r.total_bytes, "size": r.size, "status": r.status, "description": r.description
                        })
                    })
                    .collect(),
            )
        }
        "search_assets" => {
            let q = SearchQuery {
                query: s(args, "query").unwrap_or_default(),
                kind: s(args, "kind"),
                package: s(args, "package"),
                publisher: s(args, "publisher"),
                ext: s(args, "ext"),
                include_folders: false,
                limit: n(args, "limit").unwrap_or(30) as usize,
                offset: 0,
            };
            Value::Array(svc.search(&q)?.iter().map(slim_asset).collect())
        }
        "list_package_assets" => {
            let package = s(args, "package").ok_or_else(|| anyhow::anyhow!("package is required"))?;
            let l = svc.ls(
                &package,
                s(args, "path_prefix").as_deref(),
                s(args, "kind").as_deref(),
                Some(n(args, "limit").unwrap_or(500) as usize),
            )?;
            json!({
                "package": {"id": l.package.id, "name": l.package.name, "publisher": l.package.publisher, "entry_count": l.package.entry_count},
                "kinds": l.kinds,
                "assets": l.assets.iter().filter(|a| !a.is_folder).map(|a| json!({"guid": a.guid, "path": a.path, "kind": a.kind, "size": a.size})).collect::<Vec<_>>()
            })
        }
        "asset_info" => {
            let ident = s(args, "identifier").ok_or_else(|| anyhow::anyhow!("identifier is required"))?;
            let info = svc.info(&ident, s(args, "package").as_deref())?;
            let mut v = serde_json::to_value(&info.asset)?;
            let refs: Vec<Value> = info
                .refs
                .iter()
                .map(|r| match &r.asset {
                    Some(c) => json!({"guid": r.guid, "path": c.path, "kind": c.kind, "package": c.package}),
                    None => json!({"guid": r.guid, "unresolved": true, "label": r.label}),
                })
                .collect();
            let users: Vec<Value> = info
                .referrers
                .iter()
                .take(100)
                .map(|r| json!({"guid": r.guid, "path": r.path, "kind": r.kind, "package": r.package}))
                .collect();
            if let Some(o) = v.as_object_mut() {
                o.insert("direct_dependencies".into(), Value::Array(refs));
                o.insert("used_by".into(), Value::Array(users));
            }
            v
        }
        "dependencies" => {
            let ids = strings(args, "identifiers");
            if ids.is_empty() {
                anyhow::bail!("identifiers is required");
            }
            let cl = svc.deps(&ids, None, b(args, "include_scripts", true), n(args, "max_depth").map(|d| d as u32))?;
            let mut v = serde_json::to_value(&cl)?;
            if let Some(o) = v.as_object_mut() {
                o.remove("edges");
            }
            v
        }
        "export_assets" => {
            let ids = strings(args, "identifiers");
            if ids.is_empty() {
                anyhow::bail!("identifiers is required");
            }
            let dests: Vec<ExportDest> = [
                s(args, "project_dir").map(|p| ExportDest::Project(crate::config::expand_tilde(&p))),
                s(args, "out_dir").map(|p| ExportDest::Dir(crate::config::expand_tilde(&p))),
                s(args, "unitypackage_path").map(|p| ExportDest::UnityPackage(crate::config::expand_tilde(&p))),
            ]
            .into_iter()
            .flatten()
            .collect();
            if dests.len() != 1 {
                anyhow::bail!("pass exactly one of project_dir, out_dir, unitypackage_path");
            }
            let req = ExportRequest {
                identifiers: ids,
                package: None,
                include_deps: b(args, "include_deps", true),
                include_scripts: b(args, "include_scripts", true),
                include_folders: true,
            };
            let opts = ExportOptions {
                force: b(args, "force", false),
                dry_run: b(args, "dry_run", false),
                conflict_check: true,
            };
            let mut log = |_: &str| {};
            let out = svc.export(&req, &dests[0], &opts, &mut log)?;
            let mut v = serde_json::to_value(&out)?;
            if let Some(o) = v.as_object_mut() {
                o.remove("assets");
            }
            v
        }
        "read_text_asset" => {
            let ident = s(args, "identifier").ok_or_else(|| anyhow::anyhow!("identifier is required"))?;
            let t = svc.text(&ident, None, n(args, "max_bytes").unwrap_or(200_000) as usize)?;
            serde_json::to_value(&t)?
        }
        other => anyhow::bail!("unknown tool {other:?}"),
    })
}

fn tool_result(v: Result<Value>) -> Value {
    match v {
        Ok(v) => {
            json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}], "structuredContent": v, "isError": false})
        }
        Err(e) => {
            json!({"content": [{"type": "text", "text": format!("error: {}", crate::error::describe(&e))}], "isError": true})
        }
    }
}

pub fn run(cfg: Config) -> Result<()> {
    let svc = open_service(&cfg)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        let n = stdin.lock().read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({"jsonrpc": "2.0", "id": Value::Null, "error": {"code": -32700, "message": format!("parse error: {e}")}});
                writeln!(stdout, "{err}")?;
                stdout.flush()?;
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        if id.is_none() || id == Some(Value::Null) {
            // Notification (initialized, cancelled, ...): nothing to answer.
            continue;
        }
        let id = id.unwrap();
        let response = match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or("");
                let version = if SUPPORTED_PROTOCOLS.contains(&requested) { requested } else { SUPPORTED_PROTOCOLS[0] };
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "unity-asset-index", "version": crate::VERSION},
                    "instructions": INSTRUCTIONS
                }})
            }
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                json!({"jsonrpc": "2.0", "id": id, "result": tool_result(call_tool(&*svc, name, &args))})
            }
            "resources/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"resources": []}}),
            "prompts/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"prompts": []}}),
            _ => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}})
            }
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}
