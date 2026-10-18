//! `uai serve`: HTTP API + embedded web UI for searching, previewing and exporting assets.
//!
//! Every request opens its own SQLite connection on a blocking thread (SQLite handles the
//! concurrency; connections are cheap in WAL mode), so the async side never touches rusqlite.
//!
//! Optional auth (`--auth-token`, `--google-domain`, `--google-email`) lives in [`auth`]; the
//! middleware here applies it to everything except the UI shell, `/healthz` and the sign-in calls.

pub mod auth;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
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
use auth::{Auth, AuthConfig, Denied, Principal};

const INDEX_HTML: &str = include_str!("../../web/index.html");
const APP_JS: &str = include_str!("../../web/app.js");
const STYLE_CSS: &str = include_str!("../../web/style.css");
const VIEWER_JS: &str = include_str!("../../web/viewer.js");

/// three.js and the loaders used by the 3D preview, embedded so the UI works offline.
const VENDOR: &[(&str, &[u8])] = &[
    ("three.module.min.js", include_bytes!("../../web/vendor/three.module.min.js")),
    ("three.core.min.js", include_bytes!("../../web/vendor/three.core.min.js")),
    ("addons/loaders/FBXLoader.js", include_bytes!("../../web/vendor/addons/loaders/FBXLoader.js")),
    ("addons/loaders/OBJLoader.js", include_bytes!("../../web/vendor/addons/loaders/OBJLoader.js")),
    ("addons/loaders/GLTFLoader.js", include_bytes!("../../web/vendor/addons/loaders/GLTFLoader.js")),
    ("addons/curves/NURBSCurve.js", include_bytes!("../../web/vendor/addons/curves/NURBSCurve.js")),
    ("addons/curves/NURBSUtils.js", include_bytes!("../../web/vendor/addons/curves/NURBSUtils.js")),
    ("addons/libs/fflate.module.js", include_bytes!("../../web/vendor/addons/libs/fflate.module.js")),
    ("addons/utils/BufferGeometryUtils.js", include_bytes!("../../web/vendor/addons/utils/BufferGeometryUtils.js")),
    ("addons/controls/OrbitControls.js", include_bytes!("../../web/vendor/addons/controls/OrbitControls.js")),
];

/// Raw assets served for in-browser preview are capped at this size.
const RAW_MAX_BYTES: usize = 256 * 1024 * 1024;

struct AppState {
    cfg: Config,
    /// `None` when the server runs open (no tokens or Google accounts configured).
    auth: Option<Arc<Auth>>,
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
async fn viewer_js() -> Response {
    static_file(VIEWER_JS, "application/javascript; charset=utf-8")
}
async fn vendor_file(Path(path): Path<String>) -> Response {
    match VENDOR.iter().find(|(p, _)| *p == path) {
        Some((_, body)) => (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("application/javascript; charset=utf-8")),
                (header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=604800")),
            ],
            *body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize, Default)]
struct IdentQuery {
    #[serde(default)]
    ident: String,
    /// raw: when `ident` is ambiguous, pick the candidate closest (by path) to this asset id
    #[serde(default)]
    near: Option<i64>,
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
        o.insert("library".into(), cfg.libraries_display().into());
        o.insert(
            "libraries".into(),
            serde_json::Value::Array(
                cfg.libraries
                    .iter()
                    .map(|p| serde_json::json!({"path": p.to_string_lossy(), "mounted": p.is_dir()}))
                    .collect(),
            ),
        );
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

fn common_prefix(a: &str, b: &str) -> usize {
    a.split('/').zip(b.split('/')).take_while(|(x, y)| x == y).count()
}

/// Resolve `ident`; if it is ambiguous and `near` is given, take the candidate whose path shares the
/// longest prefix with that asset (textures referenced by a model live next to it).
fn resolve_near(s: &LocalService, ident: &str, near: Option<i64>) -> Result<Asset> {
    match s.resolve(ident, None) {
        Ok(a) => Ok(a),
        Err(e) => {
            let Some(near_id) = near else { return Err(e) };
            let Some(UaiError::Ambiguous { candidates, .. }) = e.downcast_ref::<UaiError>() else { return Err(e) };
            let Some(anchor) = s.db.asset_by_id(near_id)? else { return Err(e) };
            let best = candidates
                .iter()
                .filter(|c| c.package_id == anchor.package_id)
                .max_by_key(|c| common_prefix(&c.path, &anchor.path))
                .or_else(|| candidates.first())
                .cloned();
            best.ok_or(e)
        }
    }
}

async fn api_meta(State(st): State<Shared>, Path(id): Path<i64>) -> ApiResult<Response> {
    let text = with_svc(&st, move |s| {
        let asset = s.resolve(&format!("#{id}"), None)?;
        let (bytes, _) = exporter::read_member(&s.cfg, &s.db, &asset, "asset.meta", 8 << 20)?
            .ok_or_else(|| UaiError::NotFound("asset.meta not found in package".to_string()))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
    .await?;
    Ok(([(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"))], text).into_response())
}

async fn raw_response(st: &Shared, ident: String, near: Option<i64>) -> ApiResult<Response> {
    let (asset, bytes, truncated) = with_svc(st, move |s| {
        let asset = resolve_near(s, &ident, near)?;
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
    raw_response(&st, ident, q.near).await
}

async fn api_raw_by_id(State(st): State<Shared>, Path(id): Path<i64>) -> ApiResult<Response> {
    raw_response(&st, format!("#{id}"), None).await
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

// ----- auth ---------------------------------------------------------------------------------------

/// Reachable without credentials: the UI shell (so it can show the sign-in form), the health check
/// and the sign-in calls themselves.
fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/" | "/app.js" | "/style.css" | "/viewer.js" | "/healthz" | "/api/auth" | "/api/login" | "/api/logout"
    ) || path.starts_with("/vendor/")
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, cred) = v.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| cred.trim().to_string())
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let prefix = format!("{}=", auth::SESSION_COOKIE);
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(prefix.as_str()).map(str::to_string))
}

/// Who is calling. A bearer credential wins over the session cookie.
async fn authenticate(auth: &Arc<Auth>, headers: &HeaderMap) -> std::result::Result<Principal, Denied> {
    if let Some(cred) = bearer(headers) {
        return check_bearer(auth, cred).await;
    }
    session_cookie(headers).and_then(|c| auth.check_session(&c)).ok_or(Denied::Missing)
}

/// Bearer checks can fetch Google's keys, so they run on a blocking thread.
async fn check_bearer(auth: &Arc<Auth>, cred: String) -> std::result::Result<Principal, Denied> {
    let auth = auth.clone();
    tokio::task::spawn_blocking(move || auth.check_bearer(&cred))
        .await
        .unwrap_or_else(|e| Err(Denied::Unavailable(format!("auth worker panicked: {e}"))))
}

fn denied(d: Denied) -> Response {
    let status = StatusCode::from_u16(d.status()).unwrap_or(StatusCode::UNAUTHORIZED);
    let body = ApiError { error: d.message(), candidates: vec![], package_candidates: vec![] };
    let mut resp = (status, Json(body)).into_response();
    if status == StatusCode::UNAUTHORIZED {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"uai\""));
    }
    resp
}

async fn require_auth(State(state): State<Shared>, mut req: Request, next: Next) -> Response {
    let Some(auth) = state.auth.clone() else { return next.run(req).await };
    if is_public(req.uri().path()) {
        return next.run(req).await;
    }
    match authenticate(&auth, req.headers()).await {
        Ok(who) => {
            req.extensions_mut().insert(who);
            next.run(req).await
        }
        Err(d) => denied(d),
    }
}

fn session_cookie_header(value: &str, max_age: u64, headers: &HeaderMap) -> HeaderValue {
    let https =
        headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("https"));
    let cookie = format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        auth::SESSION_COOKIE,
        if https { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&cookie).unwrap_or(HeaderValue::from_static(""))
}

#[derive(serde::Serialize)]
struct AuthStatus {
    required: bool,
    methods: Vec<&'static str>,
    user: Option<String>,
}

async fn api_auth(State(state): State<Shared>, headers: HeaderMap) -> Json<AuthStatus> {
    let Some(auth) = state.auth.clone() else {
        return Json(AuthStatus { required: false, methods: vec![], user: None });
    };
    let mut methods = vec![];
    if !auth.config().tokens.is_empty() {
        methods.push("token");
    }
    if auth.config().google_enabled() {
        methods.push("google");
    }
    let user = authenticate(&auth, &headers).await.ok().map(|p| p.0);
    Json(AuthStatus { required: true, methods, user })
}

#[derive(Deserialize, Default)]
struct LoginBody {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    ticket: Option<String>,
}

/// Exchange a credential for a session cookie: a one-time `ticket` (from `uai web`), a pasted
/// `token`, or the request's bearer header.
async fn api_login(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(auth) = state.auth.clone() else {
        return Json(serde_json::json!({"user": null, "required": false})).into_response();
    };
    let body: LoginBody = serde_json::from_slice(&body).unwrap_or_default();
    let who = if let Some(t) = body.ticket.filter(|t| !t.is_empty()) {
        auth.redeem_ticket(&t)
            .ok_or_else(|| Denied::Invalid("sign-in link expired or already used (run `uai web` again)".into()))
    } else if let Some(token) = body.token.filter(|t| !t.trim().is_empty()) {
        check_bearer(&auth, token).await
    } else {
        authenticate(&auth, &headers).await
    };
    match who {
        Ok(who) => {
            let (value, ttl) = auth.issue_session(&who);
            let mut resp = Json(serde_json::json!({"user": who.0})).into_response();
            resp.headers_mut().insert(header::SET_COOKIE, session_cookie_header(&value, ttl, &headers));
            resp
        }
        Err(d) => denied(d),
    }
}

/// A single-use sign-in code for the browser (`uai web`); the caller has already authenticated.
async fn api_login_ticket(State(state): State<Shared>, req: Request) -> Response {
    let Some(auth) = state.auth.clone() else { return Json(serde_json::json!({"ticket": null})).into_response() };
    match req.extensions().get::<Principal>() {
        Some(who) => Json(serde_json::json!({
            "ticket": auth.issue_ticket(who),
            "expires_in": auth::TICKET_TTL.as_secs(),
        }))
        .into_response(),
        None => denied(Denied::Missing),
    }
}

async fn api_logout(headers: HeaderMap) -> Response {
    let mut resp = Json(serde_json::json!({"user": null})).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, session_cookie_header("", 0, &headers));
    resp
}

async fn healthz() -> &'static str {
    "ok"
}

/// The API and UI without auth (LAN use, tests).
pub fn router(cfg: Config) -> Router {
    router_with_auth(cfg, None)
}

pub fn router_with_auth(cfg: Config, auth: Option<Arc<Auth>>) -> Router {
    let state: Shared = Arc::new(AppState { cfg, auth: auth.filter(|a| a.enabled()) });
    Router::new()
        .route("/", get(index_html))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/viewer.js", get(viewer_js))
        .route("/vendor/{*path}", get(vendor_file))
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
        .route("/api/assets/{id}/meta", get(api_meta))
        .route("/api/export/plan", post(api_export_plan))
        .route("/api/export/unitypackage", post(api_export_unitypackage))
        .route("/healthz", get(healthz))
        .route("/api/auth", get(api_auth))
        .route("/api/login", post(api_login))
        .route("/api/login/ticket", post(api_login_ticket))
        .route("/api/logout", post(api_logout))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024))
        .with_state(state)
}

pub fn serve(cfg: Config, bind: &str, open: bool, auth_cfg: AuthConfig) -> Result<()> {
    if cfg.server.is_some() {
        anyhow::bail!("`uai serve` needs the local index; drop --server (or use --local)");
    }
    // Fail early with a clear message if the index cannot be opened.
    let stats = LocalService::open(&cfg)?.stats()?;
    let auth = if auth_cfg.enabled() { Some(Arc::new(Auth::new(auth_cfg, &cfg.home)?)) } else { None };
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
            cfg.libraries_display(),
            if cfg.library_mounted() {
                "mounted"
            } else {
                "NOT mounted, exports/previews of unindexed thumbnails will fail"
            }
        );
        match &auth {
            Some(a) => eprintln!("auth: {}", a.config().describe()),
            None if !local.ip().is_loopback() => eprintln!(
                "warning: no authentication; anyone who can reach {url} can read and download the library \
                 (see --auth-token, --google-domain)"
            ),
            None => {}
        }
        if open {
            let _ = crate::open_browser(&url);
        }
        axum::serve(listener, router_with_auth(cfg, auth))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                eprintln!("shutting down");
            })
            .await?;
        Ok(())
    })
}
