//! Remote mode: run the read-only commands (and exports) against a `uai serve` instance over HTTP,
//! so client machines do not need the SMB share mounted or an index of their own.

use std::collections::HashMap;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use ureq::Agent;

use crate::error::UaiError;
use crate::exporter;
use crate::model::*;
use crate::service::{validate_project, ExportDest, ExportOptions, Service};

pub struct RemoteService {
    pub base: String,
    agent: Agent,
}

#[derive(serde::Serialize)]
struct DepsBody<'a> {
    identifiers: &'a [String],
    package: Option<&'a str>,
    include_scripts: bool,
    max_depth: Option<u32>,
}

impl RemoteService {
    pub fn new(base: &str) -> Result<RemoteService> {
        let base = base.trim().trim_end_matches('/').to_string();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err(
                UaiError::Invalid(format!("server URL must start with http:// or https:// (got {base:?})")).into()
            );
        }
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(3600)))
            .user_agent(format!("uai/{}", crate::VERSION))
            .build();
        Ok(RemoteService { base, agent: config.into() })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn check(&self, resp: &mut ureq::http::Response<ureq::Body>) -> Result<()> {
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        let api: ApiError = serde_json::from_str(&text).unwrap_or_else(|_| ApiError {
            error: format!("server returned HTTP {status}: {}", text.trim()),
            candidates: vec![],
            package_candidates: vec![],
        });
        Err(UaiError::from_api(status, api).into())
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let mut req = self.agent.get(self.url(path));
        for (k, v) in query {
            if !v.is_empty() {
                req = req.query(*k, v);
            }
        }
        let mut resp = req.call().with_context(|| format!("GET {}{}", self.base, path))?;
        self.check(&mut resp)?;
        Ok(resp.body_mut().with_config().limit(1 << 30).read_json::<T>()?)
    }

    fn post_json<B: serde::Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        let mut resp =
            self.agent.post(self.url(path)).send_json(body).with_context(|| format!("POST {}{}", self.base, path))?;
        self.check(&mut resp)?;
        Ok(resp.body_mut().with_config().limit(1 << 30).read_json::<T>()?)
    }

    fn get_bytes(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<u8>> {
        let mut req = self.agent.get(self.url(path));
        for (k, v) in query {
            if !v.is_empty() {
                req = req.query(*k, v);
            }
        }
        let mut resp = req.call().with_context(|| format!("GET {}{}", self.base, path))?;
        self.check(&mut resp)?;
        Ok(resp.body_mut().with_config().limit(1 << 30).read_to_vec()?)
    }

    /// Ask the server to build a slim .unitypackage for `req` and stream it into `out`.
    fn download_package(&self, req: &ExportRequest, out: &Path, log: &mut dyn FnMut(&str)) -> Result<u64> {
        log(&format!("downloading package from {}", self.base));
        let mut resp = self
            .agent
            .post(self.url("/api/export/unitypackage"))
            .send_json(req)
            .with_context(|| format!("POST {}/api/export/unitypackage", self.base))?;
        self.check(&mut resp)?;
        let mut reader = resp.body_mut().as_reader();
        let mut file =
            BufWriter::new(std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?);
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            total += n as u64;
        }
        file.flush()?;
        Ok(total)
    }

    fn pkg_query(package: Option<&str>) -> Vec<(&'static str, String)> {
        vec![("package", package.unwrap_or("").to_string())]
    }
}

impl Service for RemoteService {
    fn is_remote(&self) -> bool {
        true
    }

    fn stats(&self) -> Result<Stats> {
        self.get_json("/api/stats", &[])
    }

    fn packages(&self) -> Result<Vec<Package>> {
        self.get_json("/api/packages", &[])
    }

    fn find_package(&self, needle: &str) -> Result<Package> {
        self.get_json("/api/package", &[("q", needle.to_string())])
    }

    fn search(&self, q: &SearchQuery) -> Result<Vec<Asset>> {
        self.get_json(
            "/api/search",
            &[
                ("q", q.query.clone()),
                ("kind", q.kind.clone().unwrap_or_default()),
                ("package", q.package.clone().unwrap_or_default()),
                ("publisher", q.publisher.clone().unwrap_or_default()),
                ("ext", q.ext.clone().unwrap_or_default()),
                ("folders", if q.include_folders { "1".into() } else { String::new() }),
                ("limit", q.limit.to_string()),
                ("offset", if q.offset > 0 { q.offset.to_string() } else { String::new() }),
            ],
        )
    }

    fn ls(
        &self,
        package: &str,
        prefix: Option<&str>,
        kind: Option<&str>,
        limit: Option<usize>,
    ) -> Result<PackageListing> {
        self.get_json(
            "/api/ls",
            &[
                ("package", package.to_string()),
                ("prefix", prefix.unwrap_or("").to_string()),
                ("kind", kind.unwrap_or("").to_string()),
                ("limit", limit.map(|l| l.to_string()).unwrap_or_default()),
            ],
        )
    }

    fn resolve(&self, ident: &str, package: Option<&str>) -> Result<Asset> {
        let mut q = Self::pkg_query(package);
        q.push(("ident", ident.to_string()));
        self.get_json("/api/resolve", &q)
    }

    fn info(&self, ident: &str, package: Option<&str>) -> Result<AssetInfo> {
        let mut q = Self::pkg_query(package);
        q.push(("ident", ident.to_string()));
        self.get_json("/api/info", &q)
    }

    fn deps(
        &self,
        idents: &[String],
        package: Option<&str>,
        include_scripts: bool,
        max_depth: Option<u32>,
    ) -> Result<ClosureOut> {
        self.post_json("/api/deps", &DepsBody { identifiers: idents, package, include_scripts, max_depth })
    }

    fn rdeps(&self, ident: &str, package: Option<&str>, limit: Option<usize>) -> Result<Vec<Asset>> {
        let mut q = Self::pkg_query(package);
        q.push(("ident", ident.to_string()));
        q.push(("limit", limit.map(|l| l.to_string()).unwrap_or_default()));
        self.get_json("/api/rdeps", &q)
    }

    fn text(&self, ident: &str, package: Option<&str>, max_bytes: usize) -> Result<TextOut> {
        let mut q = Self::pkg_query(package);
        q.push(("ident", ident.to_string()));
        q.push(("max_bytes", max_bytes.to_string()));
        self.get_json("/api/text", &q)
    }

    fn preview(&self, ident: &str, package: Option<&str>) -> Result<Vec<u8>> {
        let mut q = Self::pkg_query(package);
        q.push(("ident", ident.to_string()));
        self.get_bytes("/api/preview", &q)
    }

    fn plan(&self, req: &ExportRequest) -> Result<PlanOut> {
        self.post_json("/api/export/plan", req)
    }

    fn export(
        &self,
        req: &ExportRequest,
        dest: &ExportDest,
        opts: &ExportOptions,
        log: &mut dyn FnMut(&str),
    ) -> Result<ExportOut> {
        let plan: PlanOut = self.plan(req)?;
        if opts.dry_run {
            let mut result = ExportResult { dry_run: true, ..Default::default() };
            result.output = match dest {
                ExportDest::Project(p) | ExportDest::Dir(p) | ExportDest::UnityPackage(p) => {
                    p.to_string_lossy().to_string()
                }
            };
            for a in &plan.assets {
                result.written.push(WrittenFile { guid: a.guid.clone(), path: a.path.clone(), bytes: a.size });
                result.bytes_written += a.size;
            }
            return Ok(ExportOut { plan, result });
        }
        let t0 = std::time::Instant::now();
        let result = match dest {
            ExportDest::UnityPackage(file) => {
                if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)?;
                }
                let tmp = std::path::PathBuf::from(format!("{}.partial", file.to_string_lossy()));
                let bytes = self.download_package(req, &tmp, log)?;
                std::fs::rename(&tmp, file)?;
                ExportResult {
                    output: file.to_string_lossy().to_string(),
                    bytes_written: bytes as i64,
                    seconds: t0.elapsed().as_secs_f64(),
                    written: plan
                        .assets
                        .iter()
                        .map(|a| WrittenFile { guid: a.guid.clone(), path: a.path.clone(), bytes: a.size })
                        .collect(),
                    ..Default::default()
                }
            }
            ExportDest::Project(dir) | ExportDest::Dir(dir) => {
                let (root, guids) = match dest {
                    ExportDest::Project(_) => {
                        let proj = validate_project(dir)?;
                        let g = if opts.conflict_check { exporter::scan_project_guids(&proj) } else { HashMap::new() };
                        (proj, g)
                    }
                    _ => (dir.clone(), HashMap::new()),
                };
                let tmp = tempfile::Builder::new().prefix("uai-").suffix(".unitypackage").tempfile()?;
                self.download_package(req, tmp.path(), log)?;
                log(&format!("unpacking into {}", root.display()));
                let mut r = exporter::unpack_package_to_dir(tmp.path(), &plan.assets, &root, opts.force, &guids)?;
                r.seconds = t0.elapsed().as_secs_f64();
                r
            }
        };
        Ok(ExportOut { plan, result })
    }
}
