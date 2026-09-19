//! Transitive dependency resolution across the whole library (guid graph).

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use anyhow::Result;

use crate::db::Database;
use crate::model::{Asset, ClosureAsset, ClosureOut, PackageRef, Unresolved};
use crate::unitypackage::is_builtin_guid;

pub const SCRIPT_KINDS: &[&str] = &["script"];

const KNOWN_GUIDS_JSON: &str = include_str!("known_guids.json");

fn known_guids() -> &'static HashMap<String, String> {
    static T: OnceLock<HashMap<String, String>> = OnceLock::new();
    T.get_or_init(|| {
        serde_json::from_str::<HashMap<String, String>>(KNOWN_GUIDS_JSON)
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect()
    })
}

/// Labels for well-known guids that live outside asset-store packages (Unity built-ins, UPM packages).
pub fn known_guid_label(guid: &str) -> Option<String> {
    if is_builtin_guid(guid) {
        return Some("Unity built-in resource".to_string());
    }
    known_guids().get(&guid.to_ascii_lowercase()).cloned()
}

#[derive(Debug, Clone)]
pub struct Node {
    pub asset: Asset,
    pub depth: u32,
    /// guid of the referrer that first pulled this in
    pub via: Option<String>,
    /// other package ids that also contain this guid
    pub ambiguous_in: Vec<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct Closure {
    pub roots: Vec<String>,
    /// guid -> node, in discovery order (roots first).
    pub nodes: Vec<Node>,
    index: HashMap<String, usize>,
    pub unresolved: BTreeMap<String, Unresolved>,
    pub skipped_scripts: BTreeMap<String, Asset>,
    /// guid -> child guids (resolved or not)
    pub edges: BTreeMap<String, Vec<String>>,
}

impl Closure {
    pub fn get(&self, guid: &str) -> Option<&Node> {
        self.index.get(guid).map(|&i| &self.nodes[i])
    }
    pub fn contains(&self, guid: &str) -> bool {
        self.index.contains_key(guid)
    }
    fn insert(&mut self, node: Node) {
        self.index.insert(node.asset.guid.clone(), self.nodes.len());
        self.nodes.push(node);
    }
    pub fn assets(&self) -> impl Iterator<Item = &Asset> {
        self.nodes.iter().map(|n| &n.asset)
    }
    pub fn total_bytes(&self) -> i64 {
        self.nodes.iter().map(|n| n.asset.size).sum()
    }
    pub fn scripts(&self) -> Vec<&Node> {
        self.nodes.iter().filter(|n| SCRIPT_KINDS.contains(&n.asset.kind.as_str())).collect()
    }
    /// package_id -> nodes (sorted by package id)
    pub fn packages(&self) -> BTreeMap<i64, Vec<&Node>> {
        let mut out: BTreeMap<i64, Vec<&Node>> = BTreeMap::new();
        for n in &self.nodes {
            out.entry(n.asset.package_id).or_default().push(n);
        }
        out
    }

    pub fn to_out(&self, db: &Database) -> Result<ClosureOut> {
        let mut assets: Vec<ClosureAsset> = self
            .nodes
            .iter()
            .map(|n| ClosureAsset {
                asset: n.asset.clone(),
                depth: n.depth,
                via: n.via.clone(),
                also_in_packages: n.ambiguous_in.clone(),
            })
            .collect();
        assets.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.asset.path.cmp(&b.asset.path)));
        let package_ids: Vec<i64> = self.packages().keys().copied().collect();
        let mut packages = Vec::new();
        for pid in &package_ids {
            if let Some(p) = db.package_by_id(*pid)? {
                packages.push(PackageRef { id: p.id, name: p.name });
            }
        }
        Ok(ClosureOut {
            roots: self.roots.clone(),
            assets,
            unresolved: self.unresolved.values().cloned().collect(),
            skipped_scripts: self.skipped_scripts.values().cloned().collect(),
            total_bytes: self.total_bytes(),
            package_ids,
            packages,
            edges: self.edges.clone(),
        })
    }
}

fn pick(candidates: &[Asset], preferred: &[i64]) -> Asset {
    for pid in preferred {
        if let Some(c) = candidates.iter().find(|c| c.package_id == *pid) {
            return c.clone();
        }
    }
    candidates[0].clone()
}

/// BFS over guid references starting from root assets.
pub fn resolve_closure(
    db: &Database,
    roots: &[Asset],
    include_scripts: bool,
    max_depth: Option<u32>,
) -> Result<Closure> {
    let mut cl = Closure::default();
    let mut queue: Vec<(Asset, u32)> = Vec::new();
    let mut selected_pkgs: Vec<i64> = Vec::new();
    for r in roots {
        if cl.contains(&r.guid) {
            continue;
        }
        cl.roots.push(r.guid.clone());
        cl.insert(Node { asset: r.clone(), depth: 0, via: None, ambiguous_in: Vec::new() });
        queue.push((r.clone(), 0));
        if !selected_pkgs.contains(&r.package_id) {
            selected_pkgs.push(r.package_id);
        }
    }
    let mut i = 0;
    while i < queue.len() {
        let (asset, depth) = queue[i].clone();
        i += 1;
        if let Some(md) = max_depth {
            if depth >= md {
                continue;
            }
        }
        let children = db.refs_of(asset.id)?;
        cl.edges.insert(asset.guid.clone(), children.clone());
        for g in children {
            if cl.contains(&g) || cl.skipped_scripts.contains_key(&g) {
                continue;
            }
            if let Some(u) = cl.unresolved.get_mut(&g) {
                u.referrers.push(asset.guid.clone());
                continue;
            }
            let cands = db.assets_by_guid(&g)?;
            if cands.is_empty() {
                cl.unresolved.insert(
                    g.clone(),
                    Unresolved { guid: g.clone(), label: known_guid_label(&g), referrers: vec![asset.guid.clone()] },
                );
                continue;
            }
            let mut preferred = vec![asset.package_id];
            preferred.extend(selected_pkgs.iter().copied());
            let chosen = pick(&cands, &preferred);
            if !include_scripts && SCRIPT_KINDS.contains(&chosen.kind.as_str()) {
                cl.skipped_scripts.insert(g.clone(), chosen);
                continue;
            }
            let others: Vec<i64> =
                cands.iter().filter(|c| c.package_id != chosen.package_id).map(|c| c.package_id).collect();
            if !selected_pkgs.contains(&chosen.package_id) {
                selected_pkgs.push(chosen.package_id);
            }
            cl.insert(Node {
                asset: chosen.clone(),
                depth: depth + 1,
                via: Some(asset.guid.clone()),
                ambiguous_in: others,
            });
            queue.push((chosen, depth + 1));
        }
    }
    Ok(cl)
}
