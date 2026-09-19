//! `uai serve`: HTTP API + embedded web UI for searching, previewing and exporting assets.
//!
//! Every request opens its own SQLite connection on a blocking thread (SQLite handles the
//! concurrency; connections are cheap in WAL mode), so the async side never touches rusqlite.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio_util::io::ReaderStream;

use crate::config::Config;
use crate::error::UaiError;
use crate::exporter;
use crate::model::*;
use crate::service::{LocalService, Service};

const INDEX_HTML: &str = include_str!("../../web/index.html");
const APP_JS: &str = include_str!("../../web/app.js");
const STYLE_CSS: &str = include_str!("../../web/style.css");

/// Raw assets served for in-browser preview are capped at this size.
const RAW_MAX_BYTES: usize = 256 * 1024 * 1024;

struct AppState {
    cfg: Config,
}

type Shared = Arc<AppState>;

pub struct ApiErr(anyhow::Error);

impl From<anyhow::Error> for ApiErr {
    fn from(e: anyhow::Error) -> Self {
        ApiErr(e)
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        let (status, body) = match self.0.downcast_ref::<UaiError>() {
            Some(u) => (StatusCode::from_u16(u.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), u.to_api()),
            None => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError { error: format!("{:#}", self.0), candidates: vec![], package_candidates: vec![] },
            ),
        };
        (status, Json(body)).into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiErr>;

async fn with_svc<T, F>(state: &Shared, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&mut LocalService) -> Result<T> + Send + 'static,
{
    let cfg = state.cfg.clone();
    let out = tokio::task::spawn_blocking(move || {
        let mut svc = LocalService::open(&cfg)?;
        f(&mut svc)
    })
    .await
    .map_err(|e| ApiErr(anyhow::anyhow!("worker panicked: {e}")))?;
    out.map_err(ApiErr)
}

fn static_file(body: &'static str, mime: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        body,
    )
        .into_response()
}

async fn index_html() -> Response {
    static_file(INDEX_HTML, "text/html; charset=utf-8")
}
async fn app_js() -> Response {
    static_file(APP_JS, "application/javascript; charset=utf-8")
}
async fn style_css() -> Response {
    static_file(STYLE_CSS, "text/css; charset=utf-8")
}

#[derive(Deserialize, Default)]
struct IdentQuery {
    #[serde(default)]
    ident: String,
    /// preview: also extract from the package when the thumbnail is not stored yet
    #[serde(default)]
    extract: Option<String>,
    #[serde(default)]
    package: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

fn pkg_opt(p: &Option<String>) -> Option<&str> {
    p.as_deref().filter(|s| !s.trim().is_empty())
}

async fn api_stats(State(st): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    let cfg = st.cfg.clone();
    let stats = with_svc(&st, |s| s.stats()).await?;
    let mut v = serde_json::to_value(&stats).map_err(|e| ApiErr(e.into()))?;
    if let Some(o) = v.as_object_mut() {
        o.insert("library".into(), cfg.library.to_string_lossy().to_string().into());
        o.insert("library_mounted".into(), cfg.library_mounted().into());
        o.insert("version".into(), crate::VERSION.into());
    }
    Ok(Json(v))
}

async fn api_packages(State(st): State<Shared>) -> ApiResult<Json<Vec<Package>>> {
    Ok(Json(with_svc(&st, |s| s.packages()).await?))
}

#[derive(Deserialize)]
struct QQuery {
    #[serde(default)]
    q: String,
}

async fn api_package(State(st): State<Shared>, Query(q): Query<QQuery>) -> ApiResult<Json<Package>> {
    Ok(Json(with_svc(&st, move |s| s.find_package(&q.q)).await?))
}

async fn api_publishers(State(st): State<Shared>) -> ApiResult<Json<Vec<String>>> {
    Ok(Json(with_svc(&st, |s| s.db.publishers()).await?))
}

async fn api_kinds(State(st): State<Shared>) -> ApiResult<Json<Vec<KindCount>>> {
    Ok(Json(with_svc(&st, |s| s.db.kind_counts(None)).await?))
}

#[derive(Deserialize, Default)]
struct SearchParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    package: Option<String>,
    #[serde(default)]
    publisher: Option<String>,
    #[serde(default)]
    ext: Option<String>,
    #[serde(default)]
    folders: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

async fn api_search(State(st): State<Shared>, Query(p): Query<SearchParams>) -> ApiResult<Json<Vec<Asset>>> {
    let q = SearchQuery {
        query: p.q,
        kind: p.kind.filter(|s| !s.is_empty()),
        package: p.package.filter(|s| !s.is_empty()),
        publisher: p.publisher.filter(|s| !s.is_empty()),
        ext: p.ext.filter(|s| !s.is_empty()),
        include_folders: matches!(p.folders.as_deref(), Some("1") | Some("true")),
        limit: p.limit.unwrap_or(50).clamp(1, 1000),
        offset: p.offset.unwrap_or(0),
    };
    Ok(Json(with_svc(&st, move |s| s.search(&q)).await?))
}

#[derive(Deserialize, Default)]
struct LsParams {
    #[serde(default)]
    package: String,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn api_ls(State(st): State<Shared>, Query(p): Query<LsParams>) -> ApiResult<Json<PackageListing>> {
    Ok(Json(with_svc(&st, move |s| s.ls(&p.package, pkg_opt(&p.prefix), pkg_opt(&p.kind), p.limit)).await?))
}

async fn api_resolve(State(st): State<Shared>, Query(q): Query<IdentQuery>) -> ApiResult<Json<Asset>> {
    Ok(Json(with_svc(&st, move |s| s.resolve(&q.ident, pkg_opt(&q.package))).await?))
}

async fn api_info(State(st): State<Shared>, Query(q): Query<IdentQuery>) -> ApiResult<Json<AssetInfo>> {
    Ok(Json(with_svc(&st, move |s| s.info(&q.ident, pkg_opt(&q.package))).await?))
}

async fn api_asset_by_id(State(st): State<Shared>, Path(id): Path<i64>) -> ApiResult<Json<AssetInfo>> {
    Ok(Json(with_svc(&st, move |s| s.info(&format!("#{id}"), None)).await?))
}

#[derive(Deserialize)]
struct DepsBody {
    identifiers: Vec<String>,
    #[serde(default)]
    package: Option<String>,
    #[serde(default = "yes")]
    include_scripts: bool,
    #[serde(default)]
    max_depth: Option<u32>,
}

fn yes() -> bool {
    true
}

async fn api_deps(State(st): State<Shared>, Json(b): Json<DepsBody>) -> ApiResult<Json<ClosureOut>> {
    Ok(Json(with_svc(&st, move |s| s.deps(&b.identifiers, pkg_opt(&b.package), b.include_scripts, b.max_depth)).await?))
}

async fn api_rdeps(State(st): State<Shared>, Query(q): Query<IdentQuery>) -> ApiResult<Json<Vec<Asset>>> {
    Ok(Json(with_svc(&st, move |s| s.rdeps(&q.ident, pkg_opt(&q.package), Some(q.limit.unwrap_or(200)))).await?))
}

async fn api_text(State(st): State<Shared>, Query(q): Query<IdentQuery>) -> ApiResult<Json<TextOut>> {
    Ok(Json(
        with_svc(&st, move |s| s.text(&q.ident, pkg_opt(&q.package), q.max_bytes.unwrap_or(2_000_000).min(64 << 20)))
            .await?,
    ))
}

fn png_response(bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("image/png")),
            (header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400")),
        ],
        bytes,
    )
        .into_response()
}

async fn preview_bytes(st: &Shared, ident: String, package: Option<String>, extract: bool) -> ApiResult<Vec<u8>> {
    with_svc(st, move |s| {
        let asset = s.resolve(&ident, pkg_opt(&package))?;
        s.preview_for(&asset, extract)?.ok_or_else(|| {
            UaiError::NotFound(if asset.has_preview {
                format!(
                    "preview of {} is not in the thumbnail store yet (run `uai index`, or request it with ?extract=1)",
                    asset.path
                )
            } else {
                format!("{} has no preview", asset.path)
            })
            .into()
        })
    })
    .await
}

fn flag(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1") | Some("true"))
}

async fn api_preview(State(st): State<Shared>, Query(q): Query<IdentQuery>) -> ApiResult<Response> {
    Ok(png_response(preview_bytes(&st, q.ident, q.package, true).await?))
}

async fn api_preview_by_id(
    State(st): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<IdentQuery>,
) -> ApiResult<Response> {
    Ok(png_response(preview_bytes(&st, format!("#{id}"), None, flag(&q.extract)).await?))
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "aif" | "aiff" => "audio/aiff",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "json" => "application/json",
        "txt" | "md" | "cs" | "shader" | "hlsl" | "cginc" | "prefab" | "mat" | "asset" | "unity" | "anim"
        | "controller" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn raw_response(st: &Shared, ident: String) -> ApiResult<Response> {
    let (asset, bytes, truncated) = with_svc(st, move |s| {
        let asset = s.resolve(&ident, None)?;
        let (bytes, truncated) = s.raw_for(&asset, RAW_MAX_BYTES)?.ok_or_else(|| {
            UaiError::NotFound("asset not found in package (index stale? run `uai index`)".to_string())
        })?;
        Ok((asset, bytes, truncated))
    })
    .await?;
    if truncated {
        return Err(ApiErr(UaiError::Invalid(format!("{} is larger than the preview limit", asset.path)).into()));
    }
    let mime = mime_for_ext(&asset.ext);
    let disposition = format!("inline; filename=\"{}\"", asset.name.replace('"', ""));
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition).unwrap_or(HeaderValue::from_static("inline")),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400")),
        ],
        bytes,
    )
        .into_response())
}

async fn api_raw(State(st): State<Shared>, Query(q): Query<IdentQuery>) -> ApiResult<Response> {
    let ident = match pkg_opt(&q.package) {
        Some(p) if !q.ident.contains("::") => format!("{p}::{}", q.ident),
        _ => q.ident,
    };
    raw_response(&st, ident).await
}

async fn api_raw_by_id(State(st): State<Shared>, Path(id): Path<i64>) -> ApiResult<Response> {
    raw_response(&st, format!("#{id}")).await
}

async fn api_export_plan(State(st): State<Shared>, Json(req): Json<ExportRequest>) -> ApiResult<Json<PlanOut>> {
    Ok(Json(with_svc(&st, move |s| s.plan(&req)).await?))
}

async fn api_export_unitypackage(State(st): State<Shared>, Json(req): Json<ExportRequest>) -> ApiResult<Response> {
    let (tmp, name, result) = with_svc(&st, move |s| {
        let (roots, plan) = s.build_plan(&req)?;
        let tmp =
            tempfile::Builder::new().prefix("uai-export-").suffix(".unitypackage").tempfile_in(std::env::temp_dir())?;
        let file = std::io::BufWriter::new(std::fs::File::create(tmp.path())?);
        let mut log = |_: &str| {};
        let result = exporter::write_unitypackage(&s.cfg, &s.db, &plan, file, &mut log)?;
        let base = roots
            .first()
            .map(|r| r.name.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or_else(|| r.name.clone()))
            .unwrap_or_else(|| "export".into());
        let name = if roots.len() > 1 {
            format!("{base}+{}.unitypackage", roots.len() - 1)
        } else {
            format!("{base}.unitypackage")
        };
        Ok((tmp, name, result))
    })
    .await?;
    let file = tokio::fs::File::open(tmp.path()).await.map_err(|e| ApiErr(e.into()))?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let stream = ReaderStream::with_capacity(file, 1 << 20);
    // Keep the temp file alive until the stream is dropped.
    let stream = futures_util::stream::StreamExt::map(stream, move |chunk| {
        let _keep = &tmp;
        chunk
    });
    let safe_name: String = name.chars().filter(|c| !matches!(c, '"' | '\\' | '\r' | '\n')).collect();
    let mut resp = Body::from_stream(stream).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&len.to_string()).unwrap());
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{safe_name}\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    if let Ok(v) = HeaderValue::from_str(&result.written.len().to_string()) {
        h.insert("x-uai-files", v);
    }
    if let Ok(v) = HeaderValue::from_str(&result.missing_in_package.len().to_string()) {
        h.insert("x-uai-missing", v);
    }
    Ok(resp)
}

pub fn router(cfg: Config) -> Router {
    let state: Shared = Arc::new(AppState { cfg });
    Router::new()
        .route("/", get(index_html))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/api/stats", get(api_stats))
        .route("/api/packages", get(api_packages))
        .route("/api/package", get(api_package))
        .route("/api/publishers", get(api_publishers))
        .route("/api/kinds", get(api_kinds))
        .route("/api/search", get(api_search))
        .route("/api/ls", get(api_ls))
        .route("/api/resolve", get(api_resolve))
        .route("/api/info", get(api_info))
        .route("/api/deps", post(api_deps))
        .route("/api/rdeps", get(api_rdeps))
        .route("/api/text", get(api_text))
        .route("/api/preview", get(api_preview))
        .route("/api/raw", get(api_raw))
        .route("/api/assets/{id}", get(api_asset_by_id))
        .route("/api/assets/{id}/preview.png", get(api_preview_by_id))
        .route("/api/assets/{id}/raw", get(api_raw_by_id))
        .route("/api/export/plan", post(api_export_plan))
        .route("/api/export/unitypackage", post(api_export_unitypackage))
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024))
        .with_state(state)
}

pub fn serve(cfg: Config, bind: &str, open: bool) -> Result<()> {
    if cfg.server.is_some() {
        anyhow::bail!("`uai serve` needs the local index; drop --server (or use --local)");
    }
    // Fail early with a clear message if the index cannot be opened.
    let stats = LocalService::open(&cfg)?.stats()?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let addr: SocketAddr = bind.parse().with_context(|| format!("invalid bind address {bind:?}"))?;
        let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
        let local = listener.local_addr()?;
        let url = format!(
            "http://{}",
            if local.ip().is_unspecified() { format!("localhost:{}", local.port()) } else { local.to_string() }
        );
        eprintln!(
            "uai {} serving {} packages / {} assets / {} previews at {url}  (library {}: {})",
            crate::VERSION,
            stats.packages,
            stats.assets,
            stats.previews,
            cfg.library.display(),
            if cfg.library_mounted() {
                "mounted"
            } else {
                "NOT mounted, exports/previews of unindexed thumbnails will fail"
            }
        );
        if open {
            let _ = open_browser(&url);
        }
        axum::serve(listener, router(cfg))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                eprintln!("shutting down");
            })
            .await?;
        Ok(())
    })
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let cmd = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let cmd = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let cmd = std::process::Command::new("xdg-open").arg(url).spawn();
    cmd.map(|_| ())
}
