//! Native Trakt integration: OAuth device-code login plus scrobble/history/
//! ratings sync. Unlike every other addon in this module this one is not a
//! Stremio-protocol addon — it implements `MediaTrackerAddon` directly, the
//! same contract `media_tracker.rs` defines for e.g. Yamtrack.
//!
//! The Trakt app's client_id/client_secret are operator-level config (see
//! `Config::trakt_client_id`/`trakt_client_secret` in `lib.rs`), not per-addon
//! options: device-code flow needs no redirect URI, so one registered Trakt
//! app can authorize any number of independent users/servers, and this
//! deployment reuses the client_id/secret already registered for Mycelium
//! rather than requiring a second app registration.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::sync::Arc;
use uuid::Uuid;

use remux_sdks::trakt::{
    self, EpisodeRef, HistoryItems, MovieRef, ScrobbleTarget, ShowRef, TraktIds, TraktUserAuth,
};
use remux_sdks::{ClientError, Endpoint};

use super::media_tracker::{
    AuthFlow, DeviceAuthPoll, DeviceAuthStart, MediaTrackerAddon, MediaTrackerCapabilities,
    MediaTrackerCredentials, MediaTrackerCtx, MediaTrackerError, MediaTrackerEvent,
    MediaTrackerEventKind, MediaTrackerResult, MediaTrackerTarget, RemoteWatch, SyncDirection,
};
use super::{
    AddonCapabilities, AddonKind, AddonMetadata, AddonPreset, AddonPresetRegistration, MediaKind,
    ResourceType,
};

const TRAKT_BASE_URL: &str = "https://api.trakt.tv/";

pub struct TraktPreset;

impl AddonPreset for TraktPreset {
    fn id(&self) -> &'static str {
        "trakt"
    }

    fn metadata(&self) -> AddonMetadata {
        AddonMetadata {
            id: "trakt".to_string(),
            display_name: "Trakt".to_string(),
            description: "Sync watch activity, ratings and history with your Trakt.tv account."
                .to_string(),
            icon: None,
            supported_resources: vec![AddonMetadata::simple_resource(ResourceType::Tracking)],
            supported_types: vec![MediaKind::Movie, MediaKind::Episode, MediaKind::Series],
            supported_resources_user: vec![],
            supported_types_user: vec![],
            // No user-facing config fields: the client_id/secret come from the
            // operator's Config, and each user authorizes their own account
            // through the device-code flow rather than typing anything here.
            options: vec![],
        }
    }

    fn from_cfg(
        &self,
        _addon_id: Uuid,
        _cfg: &serde_json::Value,
        config: &crate::Config,
    ) -> Result<AddonCapabilities> {
        let client_id = config
            .trakt_client_id
            .clone()
            .ok_or_else(|| anyhow!("TRAKT_CLIENT_ID is not configured"))?;
        let client_secret = config
            .trakt_client_secret
            .clone()
            .ok_or_else(|| anyhow!("TRAKT_CLIENT_SECRET is not configured"))?;

        let addon = Arc::new(TraktMediaTracker {
            client_id,
            client_secret,
        });
        Ok(AddonCapabilities {
            kind: Some(addon.clone()),
            media_tracker: Some(addon),
            ..Default::default()
        })
    }
}

inventory::submit! {
    AddonPresetRegistration(|| Box::new(TraktPreset))
}

pub struct TraktMediaTracker {
    client_id: String,
    client_secret: String,
}

impl AddonKind for TraktMediaTracker {
    fn id(&self) -> &'static str {
        "trakt"
    }
}

/// What `connect_with_token`/device-auth store in `UserMediaTracker.credentials`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TraktCreds {
    access_token: String,
    refresh_token: String,
}

impl TraktCreds {
    fn from_value(v: &serde_json::Value) -> MediaTrackerResult<Self> {
        serde_json::from_value(v.clone())
            .map_err(|e| MediaTrackerError::permanent(format!("malformed trakt credentials: {e}")))
    }
}

fn target_ids(ids: &crate::db::ExternalIds) -> TraktIds {
    TraktIds {
        imdb: ids.imdb.as_ref().map(|s| s.to_string()),
        tmdb: ids.tmdb,
        tvdb: ids.tvdb,
    }
}

/// Builds the movie/show+episode JSON body Trakt's sync/scrobble endpoints
/// expect for one target, honoring `Endpoint::body()`'s untagged variants.
fn scrobble_target(target: &MediaTrackerTarget, progress: f32) -> MediaTrackerResult<ScrobbleTarget> {
    match &target.kind {
        crate::db::MediaKind::Movie => Ok(ScrobbleTarget::Movie {
            movie: MovieRef {
                ids: target_ids(&target.ids),
            },
            progress,
        }),
        crate::db::MediaKind::Episode => {
            let series = target
                .series
                .as_ref()
                .ok_or_else(|| MediaTrackerError::permanent("episode has no series"))?;
            let season = target
                .season
                .ok_or_else(|| MediaTrackerError::permanent("episode has no season number"))?;
            let episode = target
                .episode
                .ok_or_else(|| MediaTrackerError::permanent("episode has no episode number"))?;
            Ok(ScrobbleTarget::Episode {
                show: ShowRef {
                    ids: target_ids(&series.ids),
                },
                episode: EpisodeRef { season, number: episode },
                progress,
            })
        }
        other => Err(MediaTrackerError::unsupported(&format!(
            "scrobbling a {other:?}"
        ))),
    }
}

/// One-item history/ratings payload for `sync/history` and `sync/ratings`,
/// mirroring `scrobble_target`'s movie/episode split.
fn history_items(
    target: &MediaTrackerTarget,
    rating: Option<f32>,
) -> MediaTrackerResult<HistoryItems> {
    let mut movies = Vec::new();
    let mut shows = Vec::new();
    match &target.kind {
        crate::db::MediaKind::Movie => {
            let mut v = serde_json::json!({ "ids": target_ids(&target.ids) });
            if let Some(r) = rating {
                v["rating"] = serde_json::json!(r.round() as i32);
            }
            movies.push(v);
        }
        crate::db::MediaKind::Episode => {
            let series = target
                .series
                .as_ref()
                .ok_or_else(|| MediaTrackerError::permanent("episode has no series"))?;
            let season = target
                .season
                .ok_or_else(|| MediaTrackerError::permanent("episode has no season number"))?;
            let episode = target
                .episode
                .ok_or_else(|| MediaTrackerError::permanent("episode has no episode number"))?;
            let mut ep = serde_json::json!({ "season": season, "number": episode });
            if let Some(r) = rating {
                ep["rating"] = serde_json::json!(r.round() as i32);
            }
            shows.push(serde_json::json!({
                "ids": target_ids(&series.ids),
                "seasons": [{ "number": season, "episodes": [ep] }],
            }));
        }
        other => {
            return Err(MediaTrackerError::unsupported(&format!(
                "syncing a {other:?}"
            )));
        }
    }
    Ok(HistoryItems { movies, shows })
}

/// Maps a client-side/transport failure onto the Retryable/Permanent split
/// `on_event` and friends need to report.
fn map_client_err(e: ClientError) -> MediaTrackerError {
    match e {
        ClientError::Unauthorized => MediaTrackerError::reauth("trakt rejected the token"),
        ClientError::RateLimited { retry_after_secs } => MediaTrackerError::retry_after(
            "trakt rate limit",
            std::time::Duration::from_secs(retry_after_secs),
        ),
        ClientError::Http { status, message, .. } if status == 401 || status == 403 => {
            MediaTrackerError::reauth(format!("trakt returned {status}: {message}"))
        }
        ClientError::Http { status, .. } if status >= 500 => {
            MediaTrackerError::retryable(format!("trakt returned {status}"))
        }
        other => MediaTrackerError::permanent(other.to_string()),
    }
}

impl TraktMediaTracker {
    fn user_client(
        &self,
        creds: &TraktCreds,
    ) -> MediaTrackerResult<remux_sdks::RestClient<TraktUserAuth>> {
        trakt::trakt_user_client(&self.client_id, &creds.access_token, TRAKT_BASE_URL)
            .map_err(|e| MediaTrackerError::permanent(format!("invalid trakt base url: {e}")))
    }
}

#[async_trait]
impl MediaTrackerAddon for TraktMediaTracker {
    fn capabilities(&self) -> MediaTrackerCapabilities {
        use MediaTrackerEventKind::*;
        MediaTrackerCapabilities {
            auth_flow: AuthFlow::OAuthDeviceCode,
            connect_fields: vec![],
            supported_events: vec![
                PlaybackStart,
                PlaybackProgress,
                PlaybackStop,
                MarkPlayed,
                MarkUnplayed,
                Rating,
            ],
            default_event_filter: vec![PlaybackStart, PlaybackProgress, PlaybackStop, MarkPlayed],
            history_import: true,
            progress_import: true,
            watch_state_sync: SyncDirection::Push,
            favorites: SyncDirection::None,
            ratings: SyncDirection::Push,
            watchlist: SyncDirection::None,
        }
    }

    async fn begin_device_auth(&self, _ctx: &MediaTrackerCtx) -> MediaTrackerResult<DeviceAuthStart> {
        let client = trakt::trakt_client(&self.client_id, TRAKT_BASE_URL)
            .map_err(|e| MediaTrackerError::permanent(format!("invalid trakt base url: {e}")))?;
        let code = client
            .execute(trakt::DeviceCodeEndpoint {
                client_id: self.client_id.clone(),
            })
            .await
            .map_err(map_client_err)?;
        Ok(DeviceAuthStart {
            verification_url: code.verification_url,
            user_code: code.user_code,
            poll_token: code.device_code,
            interval: std::time::Duration::from_secs(code.interval.max(1) as u64),
            expires_in: std::time::Duration::from_secs(code.expires_in.max(1) as u64),
        })
    }

    async fn poll_device_auth(
        &self,
        poll_token: &str,
        _ctx: &MediaTrackerCtx,
    ) -> MediaTrackerResult<DeviceAuthPoll> {
        let client = trakt::trakt_client(&self.client_id, TRAKT_BASE_URL)
            .map_err(|e| MediaTrackerError::permanent(format!("invalid trakt base url: {e}")))?;
        match client
            .execute(trakt::DeviceTokenEndpoint {
                device_code: poll_token.to_string(),
                client_id: self.client_id.clone(),
                client_secret: self.client_secret.clone(),
            })
            .await
        {
            Ok(token) => {
                let creds = TraktCreds {
                    access_token: token.access_token,
                    refresh_token: token.refresh_token,
                };
                Ok(DeviceAuthPoll::Approved(MediaTrackerCredentials::new(
                    serde_json::to_value(creds)
                        .map_err(|e| MediaTrackerError::permanent(e.to_string()))?,
                )))
            }
            // Trakt: 400 = authorization_pending (keep polling), 404/409/410/418
            // = expired/denied/already-used — anything else is a real failure.
            Err(ClientError::Http { status: 400, .. }) => Ok(DeviceAuthPoll::Pending),
            Err(ClientError::Http { status, .. })
                if matches!(status, 404 | 409 | 410 | 418) =>
            {
                Ok(DeviceAuthPoll::Denied)
            }
            Err(e) => Err(map_client_err(e)),
        }
    }

    async fn refresh(
        &self,
        creds: &MediaTrackerCredentials,
        _ctx: &MediaTrackerCtx,
    ) -> MediaTrackerResult<MediaTrackerCredentials> {
        let old = TraktCreds::from_value(creds.expose())?;
        let client = trakt::trakt_client(&self.client_id, TRAKT_BASE_URL)
            .map_err(|e| MediaTrackerError::permanent(format!("invalid trakt base url: {e}")))?;
        let token = client
            .execute(trakt::RefreshTokenEndpoint {
                refresh_token: old.refresh_token,
                client_id: self.client_id.clone(),
                client_secret: self.client_secret.clone(),
            })
            .await
            .map_err(map_client_err)?;
        let new = TraktCreds {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
        };
        Ok(MediaTrackerCredentials::new(
            serde_json::to_value(new).map_err(|e| MediaTrackerError::permanent(e.to_string()))?,
        ))
    }

    async fn verify(
        &self,
        creds: &MediaTrackerCredentials,
        _ctx: &MediaTrackerCtx,
    ) -> MediaTrackerResult<()> {
        let creds = TraktCreds::from_value(creds.expose())?;
        let client = self.user_client(&creds)?;
        client
            .execute(trakt::VerifyEndpoint)
            .await
            .map_err(map_client_err)?;
        Ok(())
    }

    async fn on_event(
        &self,
        event: &MediaTrackerEvent,
        target: &MediaTrackerTarget,
        creds: &MediaTrackerCredentials,
        _ctx: &MediaTrackerCtx,
    ) -> MediaTrackerResult<()> {
        let creds = TraktCreds::from_value(creds.expose())?;
        let client = self.user_client(&creds)?;

        // A movie's runtime is not on `MediaTrackerTarget`, so progress is
        // reported as a coarse 0/50/100 rather than a true percentage — good
        // enough for Trakt's own 80%-watched threshold on stop/finish, and for
        // the "currently watching" indicator start/pause update.
        match event {
            MediaTrackerEvent::PlaybackStart { .. } => {
                let t = scrobble_target(target, 1.0)?;
                client
                    .execute(trakt::ScrobbleEndpoint { action: "start", target: t })
                    .await
                    .map_err(map_client_err)?;
            }
            MediaTrackerEvent::PlaybackProgress { is_paused, .. } => {
                let t = scrobble_target(target, 50.0)?;
                let action = if *is_paused { "pause" } else { "start" };
                client
                    .execute(trakt::ScrobbleEndpoint { action, target: t })
                    .await
                    .map_err(map_client_err)?;
            }
            MediaTrackerEvent::PlaybackStop { played, .. } => {
                let progress = if *played { 100.0 } else { 0.0 };
                let t = scrobble_target(target, progress)?;
                client
                    .execute(trakt::ScrobbleEndpoint { action: "stop", target: t })
                    .await
                    .map_err(map_client_err)?;
            }
            MediaTrackerEvent::MarkPlayed => {
                let items = history_items(target, None)?;
                client
                    .execute(trakt::SyncHistoryEndpoint { remove: false, items })
                    .await
                    .map_err(map_client_err)?;
            }
            MediaTrackerEvent::MarkUnplayed => {
                let items = history_items(target, None)?;
                client
                    .execute(trakt::SyncHistoryEndpoint { remove: true, items })
                    .await
                    .map_err(map_client_err)?;
            }
            MediaTrackerEvent::Rating { rating } => {
                let items = history_items(target, *rating)?;
                client
                    .execute(trakt::SyncRatingsEndpoint {
                        remove: rating.is_none(),
                        items,
                    })
                    .await
                    .map_err(map_client_err)?;
            }
            MediaTrackerEvent::MarkFavorite | MediaTrackerEvent::UnmarkFavorite => {
                return Err(MediaTrackerError::unsupported("favorites"));
            }
        }
        Ok(())
    }

    async fn import_history(
        &self,
        creds: &MediaTrackerCredentials,
        _ctx: &MediaTrackerCtx,
    ) -> MediaTrackerResult<Vec<RemoteWatch>> {
        let creds = TraktCreds::from_value(creds.expose())?;
        let client = self.user_client(&creds)?;
        let mut out = Vec::new();
        // One reasonably-sized page: a full paginated sweep is future work,
        // this gives a new connection an immediate, useful history seed.
        for page in 1..=5u32 {
            let items = client
                .execute(trakt::GetHistoryEndpoint {
                    start_at: None,
                    page,
                    limit: 100,
                })
                .await
                .map_err(map_client_err)?;
            if items.is_empty() {
                break;
            }
            let got = items.len();
            for item in items {
                let (ids, season, episode) = match (item.movie, item.episode, item.show) {
                    (Some(m), _, _) => (m.ids, None, None),
                    (_, Some(e), Some(s)) => (s.ids, Some(e.season), Some(e.number)),
                    _ => continue,
                };
                out.push(RemoteWatch {
                    ids: crate::db::ExternalIds {
                        imdb: ids.imdb.and_then(|s| crate::db::NonEmptyString::try_new(s).ok()),
                        tmdb: ids.tmdb,
                        tvdb: ids.tvdb,
                        ..Default::default()
                    },
                    season,
                    episode,
                    watched: true,
                    position_ticks: None,
                    watched_at: item.watched_at.map(|d| d.naive_utc()),
                    favorite: None,
                    rating: None,
                });
            }
            if got < 100 {
                break;
            }
        }
        Ok(out)
    }
}
