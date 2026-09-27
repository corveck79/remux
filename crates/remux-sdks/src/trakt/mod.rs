use crate::{Auth, Endpoint, RestClient};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct TraktAuth {
    pub client_id: String,
}

impl Auth for TraktAuth {
    fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("trakt-api-key", &self.client_id)
            .header("trakt-api-version", "2")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", "Mozilla/5.0 (compatible; remux/1.0)")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktItemIds {
    pub imdb: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktPopularItem {
    pub ids: TraktItemIds,
}

#[derive(Debug, Clone, Serialize)]
pub struct PopularParams {
    pub limit: u32,
}

#[derive(Debug, Clone)]
pub struct MoviePopularEndpoint {
    pub limit: u32,
}

impl Endpoint for MoviePopularEndpoint {
    type Output = Vec<TraktPopularItem>;

    fn path(&self) -> String {
        "movies/popular".to_string()
    }

    fn query_params(&self) -> impl serde::Serialize + '_ {
        PopularParams { limit: self.limit }
    }
}

#[derive(Debug, Clone)]
pub struct ShowPopularEndpoint {
    pub limit: u32,
}

impl Endpoint for ShowPopularEndpoint {
    type Output = Vec<TraktPopularItem>;

    fn path(&self) -> String {
        "shows/popular".to_string()
    }

    fn query_params(&self) -> impl serde::Serialize + '_ {
        PopularParams { limit: self.limit }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktStats {
    pub watchers: u64,
    pub recommended: u64,
    pub favorited: u64,
}

impl TraktStats {
    pub fn raw_score(&self) -> f64 {
        self.watchers as f64
            + self.recommended as f64 * 20.0
            + self.favorited as f64 * 10.0
    }
}

#[derive(Debug, Clone)]
pub struct MovieStatsEndpoint {
    pub imdb_id: String,
}

impl Endpoint for MovieStatsEndpoint {
    type Output = TraktStats;

    fn path(&self) -> String {
        format!("movies/{}/stats", self.imdb_id)
    }

    fn query_params(&self) -> impl serde::Serialize + '_ {
        ()
    }
}

#[derive(Debug, Clone)]
pub struct ShowStatsEndpoint {
    pub imdb_id: String,
}

impl Endpoint for ShowStatsEndpoint {
    type Output = TraktStats;

    fn path(&self) -> String {
        format!("shows/{}/stats", self.imdb_id)
    }

    fn query_params(&self) -> impl serde::Serialize + '_ {
        ()
    }
}

pub fn trakt_client(
    client_id: &str,
    base_url: &str,
) -> Result<RestClient<TraktAuth>, url::ParseError> {
    Ok(RestClient::new(base_url)?
        .with_auth(TraktAuth {
            client_id: client_id.to_string(),
        })
        .with_retry(crate::ExponentialBackoff::builder().build_with_max_retries(3)))
}

// ---------------------------------------------------------------------------
// Additions: OAuth device-code flow + scrobble/sync endpoints for the media
// tracker integration (see addons/trakt.rs).
// ---------------------------------------------------------------------------
use crate::Body;
use http::Method;
use serde_json::json;

// ---- device-code auth ----

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktDeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub expires_in: i64,
    pub interval: i64,
}

#[derive(Debug, Clone)]
pub struct DeviceCodeEndpoint {
    pub client_id: String,
}

impl Endpoint for DeviceCodeEndpoint {
    type Output = TraktDeviceCode;
    fn path(&self) -> String {
        "oauth/device/code".to_string()
    }
    fn method(&self) -> Method {
        Method::POST
    }
    fn body(&self) -> Body {
        Body::Json(json!({ "client_id": self.client_id }))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktToken {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: i64,
}

/// Trakt answers 400 while the user has not finished the browser step yet.
/// The caller (`TraktMediaTracker::poll_device_auth`) reads that specific
/// `ClientError::Http { status: 400, .. }` as "still pending", not an error.
#[derive(Debug, Clone)]
pub struct DeviceTokenEndpoint {
    pub device_code: String,
    pub client_id: String,
    pub client_secret: String,
}

impl Endpoint for DeviceTokenEndpoint {
    type Output = TraktToken;
    fn path(&self) -> String {
        "oauth/device/token".to_string()
    }
    fn method(&self) -> Method {
        Method::POST
    }
    fn body(&self) -> Body {
        Body::Json(json!({
            "code": self.device_code,
            "client_id": self.client_id,
            "client_secret": self.client_secret,
        }))
    }
}

#[derive(Debug, Clone)]
pub struct RefreshTokenEndpoint {
    pub refresh_token: String,
    pub client_id: String,
    pub client_secret: String,
}

impl Endpoint for RefreshTokenEndpoint {
    type Output = TraktToken;
    fn path(&self) -> String {
        "oauth/token".to_string()
    }
    fn method(&self) -> Method {
        Method::POST
    }
    fn body(&self) -> Body {
        Body::Json(json!({
            "refresh_token": self.refresh_token,
            "client_id": self.client_id,
            "client_secret": self.client_secret,
            "grant_type": "refresh_token",
        }))
    }
}

/// Bearer auth for calls made on a connected user's behalf, layered on top
/// of `TraktAuth`'s client-id headers (Trakt wants both on every call).
#[derive(Clone, Debug)]
pub struct TraktUserAuth {
    pub client_id: String,
    pub access_token: String,
}

impl crate::Auth for TraktUserAuth {
    fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("trakt-api-key", &self.client_id)
            .header("trakt-api-version", "2")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", "Mozilla/5.0 (compatible; remux/1.0)")
            .bearer_auth(&self.access_token)
    }
}

pub fn trakt_user_client(
    client_id: &str,
    access_token: &str,
    base_url: &str,
) -> Result<crate::RestClient<TraktUserAuth>, url::ParseError> {
    Ok(crate::RestClient::new(base_url)?
        .with_auth(TraktUserAuth {
            client_id: client_id.to_string(),
            access_token: access_token.to_string(),
        })
        .with_retry(crate::ExponentialBackoff::builder().build_with_max_retries(2)))
}

/// GET /users/settings — cheapest call that proves a token still works.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktUserSettings {
    pub user: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct VerifyEndpoint;

impl Endpoint for VerifyEndpoint {
    type Output = TraktUserSettings;
    fn path(&self) -> String {
        "users/settings".to_string()
    }
}

// ---- ids Trakt matches media on ----

#[derive(Debug, Clone, Serialize, Default)]
pub struct TraktIds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imdb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvdb: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ScrobbleTarget {
    Movie {
        movie: MovieRef,
        progress: f32,
    },
    Episode {
        show: ShowRef,
        episode: EpisodeRef,
        progress: f32,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct MovieRef {
    pub ids: TraktIds,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShowRef {
    pub ids: TraktIds,
}

#[derive(Debug, Clone, Serialize)]
pub struct EpisodeRef {
    pub season: i64,
    pub number: i64,
}

// ---- scrobble (start/pause/stop) — the "now watching" endpoints ----

#[derive(Debug, Clone)]
pub struct ScrobbleEndpoint {
    pub action: &'static str, // "start" | "pause" | "stop"
    pub target: ScrobbleTarget,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScrobbleResult {
    pub id: Option<i64>,
    pub action: Option<String>,
    pub progress: Option<f32>,
}

impl Endpoint for ScrobbleEndpoint {
    type Output = ScrobbleResult;
    fn path(&self) -> String {
        format!("scrobble/{}", self.action)
    }
    fn method(&self) -> Method {
        Method::POST
    }
    fn body(&self) -> Body {
        Body::Json(serde_json::to_value(&self.target).unwrap_or_default())
    }
}

// ---- history (mark played / unmark, and importing it back) ----

#[derive(Debug, Clone, Serialize)]
pub struct HistoryItems {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub movies: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shows: Vec<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct SyncHistoryEndpoint {
    pub remove: bool,
    pub items: HistoryItems,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SyncResult {
    #[serde(default)]
    pub added: serde_json::Value,
    #[serde(default)]
    pub deleted: serde_json::Value,
    #[serde(default)]
    pub not_found: serde_json::Value,
}

impl Endpoint for SyncHistoryEndpoint {
    type Output = SyncResult;
    fn path(&self) -> String {
        if self.remove {
            "sync/history/remove".to_string()
        } else {
            "sync/history".to_string()
        }
    }
    fn method(&self) -> Method {
        Method::POST
    }
    fn body(&self) -> Body {
        Body::Json(serde_json::to_value(&self.items).unwrap_or_default())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktHistoryItem {
    pub watched_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(rename = "type")]
    pub kind: String,
    pub movie: Option<TraktMovieDto>,
    pub episode: Option<TraktEpisodeDto>,
    pub show: Option<TraktShowRefDto>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktMovieDto {
    pub ids: TraktIdsDto,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktShowRefDto {
    pub ids: TraktIdsDto,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktEpisodeDto {
    pub season: i64,
    pub number: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TraktIdsDto {
    pub imdb: Option<String>,
    pub tmdb: Option<i64>,
    pub tvdb: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct GetHistoryEndpoint {
    pub start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub page: u32,
    pub limit: u32,
}

impl Endpoint for GetHistoryEndpoint {
    type Output = Vec<TraktHistoryItem>;
    fn path(&self) -> String {
        "sync/history".to_string()
    }
    fn query_params(&self) -> impl serde::Serialize + '_ {
        let mut q = vec![
            ("page".to_string(), self.page.to_string()),
            ("limit".to_string(), self.limit.to_string()),
        ];
        if let Some(t) = &self.start_at {
            q.push(("start_at".to_string(), t.to_rfc3339()));
        }
        q
    }
}

// ---- ratings ----

#[derive(Debug, Clone)]
pub struct SyncRatingsEndpoint {
    pub remove: bool,
    pub items: HistoryItems,
}

impl Endpoint for SyncRatingsEndpoint {
    type Output = SyncResult;
    fn path(&self) -> String {
        if self.remove {
            "sync/ratings/remove".to_string()
        } else {
            "sync/ratings".to_string()
        }
    }
    fn method(&self) -> Method {
        Method::POST
    }
    fn body(&self) -> Body {
        Body::Json(serde_json::to_value(&self.items).unwrap_or_default())
    }
}

// ---- watchlist ----

#[derive(Debug, Clone)]
pub struct GetWatchlistEndpoint;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktWatchlistItem {
    #[serde(rename = "type")]
    pub kind: String,
    pub movie: Option<TraktMovieDto>,
    pub show: Option<TraktShowRefDto>,
}

impl Endpoint for GetWatchlistEndpoint {
    type Output = Vec<TraktWatchlistItem>;
    fn path(&self) -> String {
        "sync/watchlist".to_string()
    }
}

#[derive(Debug, Clone)]
pub struct SyncWatchlistEndpoint {
    pub remove: bool,
    pub items: HistoryItems,
}

impl Endpoint for SyncWatchlistEndpoint {
    type Output = SyncResult;
    fn path(&self) -> String {
        if self.remove {
            "sync/watchlist/remove".to_string()
        } else {
            "sync/watchlist".to_string()
        }
    }
    fn method(&self) -> Method {
        Method::POST
    }
    fn body(&self) -> Body {
        Body::Json(serde_json::to_value(&self.items).unwrap_or_default())
    }
}
