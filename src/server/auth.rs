//! Optional authentication for `uai serve`: shared bearer tokens and/or Google ID tokens.
//!
//! A request gets through when it carries `Authorization: Bearer <credential>` with one of the
//! configured tokens or a Google ID token of an allowed account, or the session cookie the server
//! issues on `POST /api/login` (what the web UI uses, since `<img src>` cannot send headers).
//! With nothing configured the server stays open, as before.
//!
//! Google ID tokens are verified locally: RS256 signature against Google's published keys (cached
//! for as long as Google's `Cache-Control` says), issuer, audience, expiry, `email_verified`, then the
//! account must match an allowed Workspace domain (`hd` claim) or email.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{RsaPublicKeyComponents, RSA_PKCS1_2048_8192_SHA256};
use serde::Deserialize;

/// `aud` of the ID tokens `gcloud auth print-identity-token` mints for user accounts (the gcloud
/// CLI's own OAuth client), so it is the default accepted audience.
pub const GCLOUD_CLIENT_ID: &str = "32555940559.apps.googleusercontent.com";
pub const GOOGLE_CERTS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";
const GOOGLE_ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];
pub const SESSION_COOKIE: &str = "uai_session";
/// Clock skew tolerated on `exp` / `iat`.
const LEEWAY_SECS: u64 = 60;
/// One-time sign-in links (`uai web`) are valid this long.
pub const TICKET_TTL: Duration = Duration::from_secs(60);
/// An unknown key id triggers a refetch of Google's keys at most this often.
const REFETCH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// Shared bearer tokens; any one of them is accepted.
    pub tokens: Vec<String>,
    /// Google Workspace domains whose accounts are accepted (`hd` claim).
    pub google_domains: Vec<String>,
    /// Individual Google accounts (users or service accounts) that are accepted.
    pub google_emails: Vec<String>,
    /// Accepted `aud` values; empty means [`GCLOUD_CLIENT_ID`].
    pub google_audiences: Vec<String>,
    /// Lifetime of a web UI session cookie.
    pub session_hours: u64,
}

impl AuthConfig {
    pub fn enabled(&self) -> bool {
        !self.tokens.is_empty() || self.google_enabled()
    }

    pub fn google_enabled(&self) -> bool {
        !self.google_domains.is_empty() || !self.google_emails.is_empty()
    }

    /// One line for the startup banner (never includes the tokens).
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.tokens.is_empty() {
            parts.push(format!("{} shared token(s)", self.tokens.len()));
        }
        if self.google_enabled() {
            let who: Vec<String> =
                self.google_domains.iter().map(|d| format!("@{d}")).chain(self.google_emails.iter().cloned()).collect();
            parts.push(format!("Google accounts {}", who.join(", ")));
        }
        if parts.is_empty() {
            "none".into()
        } else {
            parts.join(" + ")
        }
    }

    fn audiences(&self) -> Vec<&str> {
        if self.google_audiences.is_empty() {
            vec![GCLOUD_CLIENT_ID]
        } else {
            self.google_audiences.iter().map(|s| s.as_str()).collect()
        }
    }
}

/// Why a request was turned away. `Missing`/`Invalid` are 401, `NotAllowed` is 403 (valid identity,
/// not on the list), `Unavailable` is 503 (Google's keys could not be fetched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    Missing,
    Invalid(String),
    NotAllowed(String),
    Unavailable(String),
}

impl Denied {
    pub fn status(&self) -> u16 {
        match self {
            Denied::Missing | Denied::Invalid(_) => 401,
            Denied::NotAllowed(_) => 403,
            Denied::Unavailable(_) => 503,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Denied::Missing => "this server requires authentication".into(),
            Denied::Invalid(why) => format!("authentication failed: {why}"),
            Denied::NotAllowed(why) => format!("not allowed: {why}"),
            Denied::Unavailable(why) => format!("cannot verify Google ID tokens right now: {why}"),
        }
    }
}

/// Who a request was authenticated as: an email for Google accounts, `token #N` for shared tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal(pub String);

/// kid -> (modulus, exponent), big-endian.
type KeyMap = HashMap<String, (Vec<u8>, Vec<u8>)>;

#[derive(Default)]
struct KeyCache {
    keys: KeyMap,
    expires: Option<Instant>,
    fetched: Option<Instant>,
}

pub struct Auth {
    cfg: AuthConfig,
    session_key: hmac::Key,
    /// HMAC of each shared token, so comparing a credential is constant-time.
    token_tags: Vec<hmac::Tag>,
    rng: SystemRandom,
    keys: Mutex<KeyCache>,
    certs_url: String,
    tickets: Mutex<HashMap<String, (Principal, Instant)>>,
}

#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
    #[serde(default)]
    kid: String,
}

#[derive(Deserialize)]
struct Claims {
    #[serde(default)]
    iss: String,
    #[serde(default)]
    aud: serde_json::Value,
    #[serde(default)]
    exp: u64,
    #[serde(default)]
    iat: u64,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    hd: Option<String>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    #[serde(default)]
    kid: String,
    #[serde(default)]
    kty: String,
    #[serde(default)]
    n: String,
    #[serde(default)]
    e: String,
}

#[derive(serde::Serialize, Deserialize)]
struct Session {
    u: String,
    exp: u64,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn decode_json<T: serde::de::DeserializeOwned>(part: &str, what: &str) -> Result<T, Denied> {
    let bytes = URL_SAFE_NO_PAD.decode(part).map_err(|_| Denied::Invalid(format!("malformed token {what}")))?;
    serde_json::from_slice(&bytes).map_err(|_| Denied::Invalid(format!("malformed token {what}")))
}

/// `max-age` from a Cache-Control header value.
fn max_age(cache_control: &str) -> Option<u64> {
    cache_control.split(',').find_map(|d| d.trim().strip_prefix("max-age=").and_then(|v| v.trim().parse().ok()))
}

impl Auth {
    /// `home` holds `session.key`, so web UI sessions survive a server restart.
    pub fn new(cfg: AuthConfig, home: &Path) -> Result<Auth> {
        let rng = SystemRandom::new();
        let key_path = home.join("session.key");
        let key_bytes = match std::fs::read(&key_path) {
            Ok(b) if b.len() >= 32 => b,
            _ => {
                let mut b = vec![0u8; 32];
                rng.fill(&mut b).map_err(|_| anyhow::anyhow!("no system randomness"))?;
                std::fs::write(&key_path, &b).with_context(|| format!("writing {}", key_path.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
                }
                b
            }
        };
        Ok(Self::with_key(cfg, &key_bytes, GOOGLE_CERTS_URL))
    }

    fn with_key(mut cfg: AuthConfig, key_bytes: &[u8], certs_url: &str) -> Auth {
        let session_key = hmac::Key::new(hmac::HMAC_SHA256, key_bytes);
        cfg.tokens.retain(|t| !t.trim().is_empty());
        cfg.google_domains =
            cfg.google_domains.iter().map(|d| d.trim().trim_start_matches('@').to_lowercase()).collect();
        cfg.google_emails = cfg.google_emails.iter().map(|e| e.trim().to_lowercase()).collect();
        cfg.google_domains.retain(|d| !d.is_empty());
        cfg.google_emails.retain(|e| !e.is_empty());
        if cfg.session_hours == 0 {
            cfg.session_hours = 12;
        }
        let token_tags = cfg.tokens.iter().map(|t| hmac::sign(&session_key, t.trim().as_bytes())).collect();
        Auth {
            cfg,
            session_key,
            token_tags,
            rng: SystemRandom::new(),
            keys: Mutex::new(KeyCache::default()),
            certs_url: certs_url.to_string(),
            tickets: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &AuthConfig {
        &self.cfg
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled()
    }

    /// Check a bearer credential: a shared token or a Google ID token. May fetch Google's keys, so
    /// call it off the async runtime.
    pub fn check_bearer(&self, credential: &str) -> Result<Principal, Denied> {
        let credential = credential.trim();
        if credential.is_empty() {
            return Err(Denied::Missing);
        }
        for (i, tag) in self.token_tags.iter().enumerate() {
            if hmac::verify(&self.session_key, credential.as_bytes(), tag.as_ref()).is_ok() {
                return Ok(Principal(format!("token #{}", i + 1)));
            }
        }
        if self.cfg.google_enabled() && credential.split('.').count() == 3 {
            return self.check_google(credential);
        }
        Err(Denied::Invalid(if self.cfg.google_enabled() {
            "not a valid token or Google ID token".into()
        } else {
            "invalid token".into()
        }))
    }

    fn check_google(&self, jwt: &str) -> Result<Principal, Denied> {
        let mut parts = jwt.split('.');
        let (h, p, s) = match (parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s)) => (h, p, s),
            _ => return Err(Denied::Invalid("malformed token".into())),
        };
        let header: JwtHeader = decode_json(h, "header")?;
        if header.alg != "RS256" {
            return Err(Denied::Invalid(format!("unsupported token algorithm {:?}", header.alg)));
        }
        let sig = URL_SAFE_NO_PAD.decode(s).map_err(|_| Denied::Invalid("malformed token signature".into()))?;
        let (n, e) = self.google_key(&header.kid)?;
        RsaPublicKeyComponents { n: &n, e: &e }
            .verify(&RSA_PKCS1_2048_8192_SHA256, format!("{h}.{p}").as_bytes(), &sig)
            .map_err(|_| Denied::Invalid("bad token signature".into()))?;

        let c: Claims = decode_json(p, "claims")?;
        let now = now_secs();
        if !GOOGLE_ISSUERS.contains(&c.iss.as_str()) {
            return Err(Denied::Invalid(format!("token issuer {:?} is not Google", c.iss)));
        }
        let auds: Vec<&str> = match &c.aud {
            serde_json::Value::String(s) => vec![s.as_str()],
            serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
            _ => vec![],
        };
        let accepted = self.cfg.audiences();
        if !auds.iter().any(|a| accepted.contains(a)) {
            return Err(Denied::Invalid(format!(
                "token audience {auds:?} is not accepted (the server expects {accepted:?}; see --google-audience)"
            )));
        }
        if c.exp + LEEWAY_SECS < now {
            return Err(Denied::Invalid("token expired (mint a new one)".into()));
        }
        if c.iat > now + LEEWAY_SECS {
            return Err(Denied::Invalid("token issued in the future (check the clock)".into()));
        }
        let email = c.email.unwrap_or_default().to_lowercase();
        if email.is_empty() {
            return Err(Denied::Invalid(
                "token has no email claim (service accounts: mint it with --include-email)".into(),
            ));
        }
        if c.email_verified == Some(false) {
            return Err(Denied::Invalid(format!("email {email} is not verified")));
        }
        let hd = c.hd.unwrap_or_default().to_lowercase();
        if self.cfg.google_emails.contains(&email) || (!hd.is_empty() && self.cfg.google_domains.contains(&hd)) {
            return Ok(Principal(email));
        }
        Err(Denied::NotAllowed(format!("{email} is not an allowed account on this server")))
    }

    fn google_key(&self, kid: &str) -> Result<(Vec<u8>, Vec<u8>), Denied> {
        {
            let cache = self.keys.lock().unwrap();
            let fresh = cache.expires.is_some_and(|t| Instant::now() < t);
            if fresh {
                if let Some(k) = cache.keys.get(kid) {
                    return Ok(k.clone());
                }
                if cache.fetched.is_some_and(|t| t.elapsed() < REFETCH_INTERVAL) {
                    return Err(Denied::Invalid("token signed with an unknown key".into()));
                }
            }
        }
        let (keys, ttl) = fetch_google_keys(&self.certs_url).map_err(|e| Denied::Unavailable(format!("{e:#}")))?;
        let mut cache = self.keys.lock().unwrap();
        cache.keys = keys;
        cache.expires = Some(Instant::now() + ttl);
        cache.fetched = Some(Instant::now());
        cache.keys.get(kid).cloned().ok_or_else(|| Denied::Invalid("token signed with an unknown key".into()))
    }

    /// A signed session cookie value for the web UI, and its lifetime in seconds.
    pub fn issue_session(&self, who: &Principal) -> (String, u64) {
        let ttl = self.cfg.session_hours * 3600;
        let body = serde_json::to_vec(&Session { u: who.0.clone(), exp: now_secs() + ttl }).unwrap_or_default();
        let payload = URL_SAFE_NO_PAD.encode(body);
        let tag = URL_SAFE_NO_PAD.encode(hmac::sign(&self.session_key, payload.as_bytes()));
        (format!("{payload}.{tag}"), ttl)
    }

    pub fn check_session(&self, value: &str) -> Option<Principal> {
        let (payload, tag) = value.split_once('.')?;
        let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
        hmac::verify(&self.session_key, payload.as_bytes(), &tag).ok()?;
        let s: Session = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        (s.exp > now_secs()).then_some(Principal(s.u))
    }

    /// A random single-use code that `POST /api/login` exchanges for a session (`uai web`), so a
    /// sign-in link never carries the credential itself.
    pub fn issue_ticket(&self, who: &Principal) -> String {
        let mut b = [0u8; 24];
        let _ = self.rng.fill(&mut b);
        let ticket = URL_SAFE_NO_PAD.encode(b);
        let mut t = self.tickets.lock().unwrap();
        t.retain(|_, (_, exp)| Instant::now() < *exp);
        t.insert(ticket.clone(), (who.clone(), Instant::now() + TICKET_TTL));
        ticket
    }

    pub fn redeem_ticket(&self, ticket: &str) -> Option<Principal> {
        let (who, exp) = self.tickets.lock().unwrap().remove(ticket)?;
        (Instant::now() < exp).then_some(who)
    }
}

/// Google's current signing keys (JWK set) and how long to cache them.
fn fetch_google_keys(url: &str) -> Result<(KeyMap, Duration)> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .user_agent(format!("uai/{}", crate::VERSION))
        .build()
        .into();
    let mut resp = agent.get(url).call().with_context(|| format!("GET {url}"))?;
    let ttl = resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .and_then(max_age)
        .unwrap_or(3600)
        .clamp(60, 86400);
    let jwks: Jwks = resp.body_mut().read_json().context("parsing Google's keys")?;
    let mut keys = HashMap::new();
    for k in jwks.keys.into_iter().filter(|k| k.kty == "RSA") {
        if let (Ok(n), Ok(e)) = (URL_SAFE_NO_PAD.decode(&k.n), URL_SAFE_NO_PAD.decode(&k.e)) {
            keys.insert(k.kid, (n, e));
        }
    }
    anyhow::ensure!(!keys.is_empty(), "no RSA keys in {url}");
    Ok((keys, Duration::from_secs(ttl)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};

    // Throwaway 2048-bit RSA key made for these tests only (`openssl genpkey`), plus its JWK n/e.
    const TEST_KEY_PKCS8_B64: &str = concat!(
        "MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQCn8fgGh7DIKBI5M7YbyGcLSz+pFGtBbWMng+kJcHKk2v2N",
        "9xpam2ozd/0et9lc3FTZDy/2nFGxm40y4DSzAU8HnW8GRIiAyuik1hAg04IyHyxKDMAt4xcUNLM9UsdU9QUqSTJBy8E56D6s",
        "pIjHgxq+6irFK1tBhM5RDmu7UoW614eMs96gfBUt+G+318hpyLtAusVL7wUWmT9sS19fNjgJr1jeT9v426oZwoUGansM+Kd6",
        "dFnriZAgy7MqMsoStALoclsAPboF4Aj4yTK60Tor/H6lZy5nuFOabJ/jlT7Qn1G3ZVCqVWjgZuFVRWNk8Dh+XV16EcAJDaQb",
        "LvlCV4n9AgMBAAECggEAAt8dC4lQ2UQozH8jvd6I/kjGiVJVDkE4OASimRLUkvxcBGGAAjBfMwsy8fj1NeuW883R5ViPjlwy",
        "LqH0T2LIAl4Zvxg+cUDcPwRhh1N9mBCvt7sT2+bGOM04/Kw2qdn8a9hWjDNzx43NId5U/ib478PXOV8nL0ooW5eM+ZCiNICb",
        "IDhre96uUtN0dlzmq63wlnq0ISkj+OLX3uYtWldfMZzdD29sNAgYsD/3UlpDAGQ5kMY2mhLEFxI3WIgGHKkKoTIdzgwseEDq",
        "07eQmu1a3U0A081xYoc1XMfDtAt4x5WGzUKqrdVB9ETO5CnBgJnl0V9gj5Phu9cOgeupNtx+AQKBgQDhAU8xFSvyYFXZdKgM",
        "mu0PXwKOJR6ZCkoZpvXN9+s5oI3Zagkk5e1BaOy+mWsT9UQlhdD21KNjO76EYkmgs+fDsggVCbwEqbQhqii5QO7GjkHHRrPO",
        "z9CMD+w+AFrX8sNdvn2yCwWRTnXLqq9fl4poaPeK09lUx2IXglmjB0a5fQKBgQC/FHhrEqZFcD8QzvH3VA/gYdFo6Qsa/Xnm",
        "Pf/9rPwasTV/ysbi9ga9fxwv842ZtjqL1I1lAQKRgRg0rUy/0ZNe6f2Nbs61IrB3jDiUUl7UyhsYqUtBV4w31+t4MXKeA4kc",
        "vL0W59LfkqFxb9nxfrD7Oc4Q01qd1a7FDobk5dD6gQKBgQCyEbE0rQa8qRUIJdShNTMLiSuihynyADyolIPP37bfQntAZzSZ",
        "aQYdUAacyRn/D4pBD+hSj/JGpI+ggZC3h+mR7KGM1Mlh/1Q5mErmYccWiJryNLg0oi7vyeIzJ2C9qriGxiz/7M0ZNDjxWEG2",
        "q120R+4whnxnJewxos2Bwk5smQKBgQC0BKYaQhyxyLkxrbYajHDUtXNjwkJ7MBkbCIhXRxTQVOmRNso/YwggmVNF/dAG476u",
        "pBZ99QClcpkKb8ah5OFRh2MgTyehUUgtqMZZPm0nTi1PA5fpvIBW0C6AMx3Hro8Lquj+XnO1vGArDfSnZvN4DBKOmYCCuKEJ",
        "99Cwb699gQKBgQCTpqQ9cbYx24o5ZnhcVxVHPypn/IpXDP3afydzRIZCKSjmlj0lS7i2kXFrQvtbUHvR7EmV+OeXiZL4/8fP",
        "oZ7UrB2VMXAi0H6s/F9rTiniq7S4w8dlj7EqXtBIuXe6zkBmEgYUw1pIyScAaoGEs1fMqlZYhwU9E1EXk7tvda/53Q==",
    );
    const TEST_N: &str = concat!(
        "p_H4BoewyCgSOTO2G8hnC0s_qRRrQW1jJ4PpCXBypNr9jfcaWptqM3f9HrfZXNxU2Q8v9pxRsZuNMuA0swFPB51vBkSIgMro",
        "pNYQINOCMh8sSgzALeMXFDSzPVLHVPUFKkkyQcvBOeg-rKSIx4MavuoqxStbQYTOUQ5ru1KFuteHjLPeoHwVLfhvt9fIaci7",
        "QLrFS-8FFpk_bEtfXzY4Ca9Y3k_b-NuqGcKFBmp7DPinenRZ64mQIMuzKjLKErQC6HJbAD26BeAI-MkyutE6K_x-pWcuZ7hT",
        "mmyf45U-0J9Rt2VQqlVo4GbhVUVjZPA4fl1dehHACQ2kGy75QleJ_Q",
    );
    const TEST_E: &str = "AQAB";
    const KID: &str = "test-kid";

    fn auth(cfg: AuthConfig) -> Auth {
        let a = Auth::with_key(cfg, &[7u8; 32], "http://127.0.0.1:9/unused");
        let mut cache = a.keys.lock().unwrap();
        cache
            .keys
            .insert(KID.into(), (URL_SAFE_NO_PAD.decode(TEST_N).unwrap(), URL_SAFE_NO_PAD.decode(TEST_E).unwrap()));
        cache.expires = Some(Instant::now() + Duration::from_secs(3600));
        cache.fetched = Some(Instant::now());
        drop(cache);
        a
    }

    fn google_cfg() -> AuthConfig {
        AuthConfig {
            google_domains: vec!["Example.com".into()],
            google_emails: vec!["robot@proj.iam.gserviceaccount.com".into()],
            ..Default::default()
        }
    }

    fn jwt(header: serde_json::Value, claims: serde_json::Value) -> String {
        let key = RsaKeyPair::from_pkcs8(&STANDARD.decode(TEST_KEY_PKCS8_B64).unwrap()).unwrap();
        let h = URL_SAFE_NO_PAD.encode(header.to_string());
        let p = URL_SAFE_NO_PAD.encode(claims.to_string());
        let mut sig = vec![0u8; key.public().modulus_len()];
        key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), format!("{h}.{p}").as_bytes(), &mut sig).unwrap();
        format!("{h}.{p}.{}", URL_SAFE_NO_PAD.encode(sig))
    }

    fn claims(email: &str, hd: Option<&str>) -> serde_json::Value {
        let now = now_secs();
        let mut c = serde_json::json!({
            "iss": "https://accounts.google.com", "aud": GCLOUD_CLIENT_ID, "azp": GCLOUD_CLIENT_ID,
            "email": email, "email_verified": true, "iat": now - 10, "exp": now + 3590, "sub": "1"
        });
        if let Some(hd) = hd {
            c["hd"] = hd.into();
        }
        c
    }

    fn header() -> serde_json::Value {
        serde_json::json!({"alg": "RS256", "kid": KID, "typ": "JWT"})
    }

    #[test]
    fn shared_tokens() {
        let a = auth(AuthConfig { tokens: vec!["s3cret".into(), "other".into()], ..Default::default() });
        assert!(a.enabled());
        assert_eq!(a.check_bearer("other"), Ok(Principal("token #2".into())));
        assert_eq!(a.check_bearer(" s3cret "), Ok(Principal("token #1".into())));
        assert!(matches!(a.check_bearer("s3cre"), Err(Denied::Invalid(_))));
        assert_eq!(a.check_bearer(""), Err(Denied::Missing));
        assert!(!auth(AuthConfig::default()).enabled());
    }

    #[test]
    fn google_domain_and_email() {
        let a = auth(google_cfg());
        assert_eq!(
            a.check_bearer(&jwt(header(), claims("Dev@Example.com", Some("example.com")))),
            Ok(Principal("dev@example.com".into()))
        );
        let sa = claims("robot@proj.iam.gserviceaccount.com", None);
        assert_eq!(a.check_bearer(&jwt(header(), sa)), Ok(Principal("robot@proj.iam.gserviceaccount.com".into())));
        let other = a.check_bearer(&jwt(header(), claims("x@gmail.com", None)));
        assert!(matches!(other, Err(Denied::NotAllowed(_))), "{other:?}");
        let wrong_hd = a.check_bearer(&jwt(header(), claims("x@evil.com", Some("evil.com"))));
        assert!(matches!(wrong_hd, Err(Denied::NotAllowed(_))));
    }

    #[test]
    fn google_rejects_bad_tokens() {
        let a = auth(google_cfg());
        let ok = claims("dev@example.com", Some("example.com"));
        let reject = |h: serde_json::Value, c: serde_json::Value| {
            let r = a.check_bearer(&jwt(h, c));
            assert!(matches!(r, Err(Denied::Invalid(_))), "{r:?}");
        };
        let mut c = ok.clone();
        c["aud"] = "someone-else".into();
        reject(header(), c);
        let mut c = ok.clone();
        c["iss"] = "https://evil.example".into();
        reject(header(), c);
        let mut c = ok.clone();
        c["exp"] = (now_secs() - 3600).into();
        reject(header(), c);
        let mut c = ok.clone();
        c["email_verified"] = false.into();
        reject(header(), c);
        reject(serde_json::json!({"alg": "none", "kid": KID}), ok.clone());
        reject(serde_json::json!({"alg": "RS256", "kid": "nope"}), ok.clone());

        // Tampered claims keep the old signature.
        let good = jwt(header(), ok.clone());
        let mut parts: Vec<String> = good.split('.').map(String::from).collect();
        let mut forged = ok.clone();
        forged["email"] = "boss@example.com".into();
        parts[1] = URL_SAFE_NO_PAD.encode(forged.to_string());
        assert!(matches!(a.check_bearer(&parts.join(".")), Err(Denied::Invalid(_))));

        // Custom audience replaces the gcloud default.
        let custom = auth(AuthConfig { google_audiences: vec!["my-aud".into()], ..google_cfg() });
        assert!(custom.check_bearer(&good).is_err());
        let mut c = ok.clone();
        c["aud"] = "my-aud".into();
        assert!(custom.check_bearer(&jwt(header(), c)).is_ok());
    }

    #[test]
    fn google_tokens_ignored_when_only_shared_tokens() {
        let a = auth(AuthConfig { tokens: vec!["t".into()], ..Default::default() });
        let r = a.check_bearer(&jwt(header(), claims("dev@example.com", Some("example.com"))));
        assert_eq!(r, Err(Denied::Invalid("invalid token".into())));
    }

    #[test]
    fn sessions_and_tickets() {
        let a = auth(google_cfg());
        let who = Principal("dev@example.com".into());
        let (cookie, ttl) = a.issue_session(&who);
        assert_eq!(ttl, 12 * 3600);
        assert_eq!(a.check_session(&cookie), Some(who.clone()));
        let (payload, tag) = cookie.split_once('.').unwrap();
        let forged = URL_SAFE_NO_PAD.encode(r#"{"u":"boss@example.com","exp":99999999999}"#);
        assert_eq!(a.check_session(&format!("{forged}.{tag}")), None);
        assert_eq!(a.check_session(payload), None);
        // A session from another server key is rejected.
        let other = Auth::with_key(google_cfg(), &[8u8; 32], "http://127.0.0.1:9/unused");
        assert_eq!(other.check_session(&cookie), None);

        let t = a.issue_ticket(&who);
        assert_eq!(a.redeem_ticket(&t), Some(who));
        assert_eq!(a.redeem_ticket(&t), None, "tickets are single-use");
    }

    #[test]
    fn cache_control_max_age() {
        assert_eq!(max_age("public, max-age=24274, must-revalidate, no-transform"), Some(24274));
        assert_eq!(max_age("no-cache"), None);
    }
}
