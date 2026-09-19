//! Error type for "user-facing" failures that the CLI, HTTP API and MCP server all need to describe
//! precisely (not found vs ambiguous vs bad request). Everything else travels as `anyhow::Error`.

use crate::model::{ApiError, Asset, Package};

#[derive(Debug, thiserror::Error)]
pub enum UaiError {
    #[error("{0}")]
    NotFound(String),
    #[error("{message}")]
    Ambiguous { message: String, candidates: Vec<Asset>, package_candidates: Vec<Package> },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Remote(String),
}

impl UaiError {
    pub fn ambiguous_assets(message: impl Into<String>, candidates: Vec<Asset>) -> Self {
        UaiError::Ambiguous { message: message.into(), candidates, package_candidates: Vec::new() }
    }
    pub fn ambiguous_packages(message: impl Into<String>, candidates: Vec<Package>) -> Self {
        UaiError::Ambiguous { message: message.into(), candidates: Vec::new(), package_candidates: candidates }
    }

    /// HTTP status the API uses for this error.
    pub fn status(&self) -> u16 {
        match self {
            UaiError::NotFound(_) => 404,
            UaiError::Ambiguous { .. } => 409,
            UaiError::Invalid(_) => 400,
            UaiError::Remote(_) => 502,
        }
    }

    pub fn to_api(&self) -> ApiError {
        match self {
            UaiError::Ambiguous { message, candidates, package_candidates } => ApiError {
                error: message.clone(),
                candidates: candidates.clone(),
                package_candidates: package_candidates.clone(),
            },
            other => ApiError { error: other.to_string(), candidates: Vec::new(), package_candidates: Vec::new() },
        }
    }

    pub fn from_api(status: u16, api: ApiError) -> Self {
        match status {
            404 => UaiError::NotFound(api.error),
            409 => UaiError::Ambiguous {
                message: api.error,
                candidates: api.candidates,
                package_candidates: api.package_candidates,
            },
            400 => UaiError::Invalid(api.error),
            _ => UaiError::Remote(api.error),
        }
    }
}

/// Render an error for the terminal, including candidate lists for ambiguous identifiers.
pub fn describe(err: &anyhow::Error) -> String {
    if let Some(UaiError::Ambiguous { message, candidates, package_candidates }) = err.downcast_ref::<UaiError>() {
        let mut s = message.clone();
        for c in candidates.iter().take(15) {
            s.push_str(&format!("\n  {}  [{}] {}", c.guid, c.package, c.path));
        }
        if candidates.len() > 15 {
            s.push_str(&format!("\n  ... {} more", candidates.len() - 15));
        }
        for p in package_candidates.iter().take(20) {
            s.push_str(&format!("\n  [{}] {}", p.id, p.name));
        }
        return s;
    }
    format!("{err:#}")
}
