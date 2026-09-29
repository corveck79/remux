//! Per-user connect/poll/list/disconnect for media-tracker addons (Trakt,
//! and any future provider implementing `MediaTrackerAddon`). Deliberately
//! generic over the addon kind — everything here is driven by whichever
//! enabled addon actually declares a `media_tracker` capability, so a second
//! provider needs no changes here, only its own `addons/<name>.rs`.
//!
//! Wire DTOs live in `remux_sdks::remux` (shared with the dashboard, which
//! calls the same endpoints through `Endpoint` structs of the same name);
//! this module only converts between those and the server-internal types in
//! `addons::media_tracker` / `db::user_media_tracker`.

use axum::{
    Json,
    extract::{Path, Query, State},
    response::IntoResponse,
};
use http::StatusCode;
use remux_macros::{delete, get, post};
use remux_sdks::remux::{
    DeviceAuthStartDto, DevicePollResultDto, DevicePollStatusDto, MediaTrackerEventKindDto,
    MediaTrackerProviderDto, MediaTrackerStatusDto,
};
use uuid::Uuid;

use crate::{
    AppState,
    addons::{
        Addon,
        media_tracker::{DeviceAuthPoll, MediaTrackerCtx, MediaTrackerEventKind},
    },
    db::{self, UserMediaTracker, auth::AuthSession},
};
use axum_anyhow::{ApiError, ApiResult as Result};

fn status_to_dto(s: db::MediaTrackerStatus) -> MediaTrackerStatusDto {
    match s {
        db::MediaTrackerStatus::Disconnected => MediaTrackerStatusDto::Disconnected,
        db::MediaTrackerStatus::Connected => MediaTrackerStatusDto::Connected,
        db::MediaTrackerStatus::Error => MediaTrackerStatusDto::Error,
        db::MediaTrackerStatus::AuthExpired => MediaTrackerStatusDto::AuthExpired,
    }
}

fn event_kind_to_dto(k: MediaTrackerEventKind) -> MediaTrackerEventKindDto {
    match k {
        MediaTrackerEventKind::PlaybackStart => MediaTrackerEventKindDto::PlaybackStart,
        MediaTrackerEventKind::PlaybackProgress => MediaTrackerEventKindDto::PlaybackProgress,
        MediaTrackerEventKind::PlaybackStop => MediaTrackerEventKindDto::PlaybackStop,
        MediaTrackerEventKind::MarkPlayed => MediaTrackerEventKindDto::MarkPlayed,
        MediaTrackerEventKind::MarkUnplayed => MediaTrackerEventKindDto::MarkUnplayed,
        MediaTrackerEventKind::MarkFavorite => MediaTrackerEventKindDto::MarkFavorite,
        MediaTrackerEventKind::UnmarkFavorite => MediaTrackerEventKindDto::UnmarkFavorite,
        MediaTrackerEventKind::Rating => MediaTrackerEventKindDto::Rating,
    }
}

fn event_kind_from_dto(k: MediaTrackerEventKindDto) -> MediaTrackerEventKind {
    match k {
        MediaTrackerEventKindDto::PlaybackStart => MediaTrackerEventKind::PlaybackStart,
        MediaTrackerEventKindDto::PlaybackProgress => MediaTrackerEventKind::PlaybackProgress,
        MediaTrackerEventKindDto::PlaybackStop => MediaTrackerEventKind::PlaybackStop,
        MediaTrackerEventKindDto::MarkPlayed => MediaTrackerEventKind::MarkPlayed,
        MediaTrackerEventKindDto::MarkUnplayed => MediaTrackerEventKind::MarkUnplayed,
        MediaTrackerEventKindDto::MarkFavorite => MediaTrackerEventKind::MarkFavorite,
        MediaTrackerEventKindDto::UnmarkFavorite => MediaTrackerEventKind::UnmarkFavorite,
        MediaTrackerEventKindDto::Rating => MediaTrackerEventKind::Rating,
    }
}

/// Every enabled addon offering `media_tracker`, joined with this user's own
/// connection row when one exists.
#[get("/media-trackers")]
pub async fn list_providers(
    State(state): State<AppState>,
    session: AuthSession,
) -> Result<impl IntoResponse> {
    let mine = UserMediaTracker::list_for_user(&state.ctx.db, session.user.id).await?;
    let runtimes = state
        .ctx
        .addons
        .list_for_user(&state.ctx.db, Some(session.user.id))
        .await;

    let out: Vec<MediaTrackerProviderDto> = runtimes
        .into_iter()
        .filter_map(|r| {
            r.caps
                .media_tracker
                .as_ref()?;
            let existing = mine
                .iter()
                .find(|t| t.addon_id == r.row.id);
            Some(MediaTrackerProviderDto {
                addon_id: r.row.id,
                kind: r
                    .row
                    .preset
                    .kind
                    .clone(),
                display_name: r
                    .caps
                    .metadata
                    .display_name
                    .clone(),
                connected: existing.is_some(),
                status: existing.map(|t| status_to_dto(t.status)),
                last_error: existing.and_then(|t| t.last_error.clone()),
                event_filters: existing
                    .map(|t| {
                        t.event_filters
                            .iter()
                            .copied()
                            .map(event_kind_to_dto)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect();
    Ok(Json(out))
}

async fn addon_or_404(state: &AppState, addon_id: Uuid) -> Result<Addon> {
    Addon::get(&state.ctx.db, addon_id)
        .await?
        .filter(|a| a.enabled)
        .ok_or_else(|| {
            ApiError::builder()
                .status(StatusCode::NOT_FOUND)
                .title("media-tracker")
                .detail("addon not found or disabled")
                .build()
        })
}

fn tctx(state: &AppState) -> MediaTrackerCtx {
    MediaTrackerCtx {
        config: std::sync::Arc::new(
            state
                .ctx
                .config
                .clone(),
        ),
    }
}

/// Starts a device-code login against the given addon. The caller polls
/// `POST /media-trackers/{addonId}/connect/device/poll` with the returned
/// `pollToken` until Trakt (or whichever provider) reports approved/denied.
#[post("/media-trackers/{addon_id}/connect/device")]
pub async fn begin_device_auth(
    State(state): State<AppState>,
    _session: AuthSession,
    Path(addon_id): Path<Uuid>,
) -> Result<impl IntoResponse> {
    let _addon = addon_or_404(&state, addon_id).await?;
    let tracker = state
        .ctx
        .addons
        .media_tracker_for(addon_id)
        .ok_or_else(|| {
            ApiError::builder()
                .status(StatusCode::BAD_REQUEST)
                .title("media-tracker")
                .detail("this addon does not support media tracking")
                .build()
        })?;
    let start = tracker
        .begin_device_auth(&tctx(&state))
        .await
        .map_err(|e| {
            ApiError::builder()
                .status(StatusCode::BAD_GATEWAY)
                .title("media-tracker")
                .detail(e.to_string())
                .build()
        })?;
    Ok(Json(DeviceAuthStartDto {
        verification_url: start.verification_url,
        user_code: start.user_code,
        poll_token: start.poll_token,
        interval_secs: start
            .interval
            .as_secs(),
        expires_in_secs: start
            .expires_in
            .as_secs(),
    }))
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DevicePollQuery {
    #[serde(rename = "pollToken")]
    pub poll_token: String,
}

/// Polled every `intervalSecs` by the dashboard. On approval this is the
/// point a `UserMediaTracker` row is actually created — nothing is persisted
/// while the user is still looking at the verification code.
#[post("/media-trackers/{addon_id}/connect/device/poll")]
pub async fn poll_device_auth(
    State(state): State<AppState>,
    session: AuthSession,
    Path(addon_id): Path<Uuid>,
    Query(q): Query<DevicePollQuery>,
) -> Result<impl IntoResponse> {
    let _addon = addon_or_404(&state, addon_id).await?;
    let tracker = state
        .ctx
        .addons
        .media_tracker_for(addon_id)
        .ok_or_else(|| {
            ApiError::builder()
                .status(StatusCode::BAD_REQUEST)
                .title("media-tracker")
                .detail("this addon does not support media tracking")
                .build()
        })?;
    let poll = tracker
        .poll_device_auth(&q.poll_token, &tctx(&state))
        .await
        .map_err(|e| {
            ApiError::builder()
                .status(StatusCode::BAD_GATEWAY)
                .title("media-tracker")
                .detail(e.to_string())
                .build()
        })?;

    let status = match poll {
        DeviceAuthPoll::Pending => DevicePollStatusDto::Pending,
        DeviceAuthPoll::Denied => DevicePollStatusDto::Denied,
        DeviceAuthPoll::Approved(creds) => {
            let caps = tracker.capabilities();
            let row = UserMediaTracker::new(
                session.user.id,
                addon_id,
                creds,
                caps.default_event_filter,
            );
            row.upsert(&state.ctx.db)
                .await?;
            DevicePollStatusDto::Connected
        }
    };
    Ok(Json(DevicePollResultDto { status }))
}

/// Disconnects the caller's own connection. Best-effort provider-side
/// revoke, matching `MediaTrackerAddon::disconnect`'s own contract: the
/// local row is deleted regardless of whether that call succeeds.
#[delete("/media-trackers/{addon_id}")]
pub async fn disconnect(
    State(state): State<AppState>,
    session: AuthSession,
    Path(addon_id): Path<Uuid>,
) -> Result<impl IntoResponse> {
    let Some(existing) =
        UserMediaTracker::get_for_user_and_addon(&state.ctx.db, session.user.id, addon_id).await?
    else {
        return Ok(StatusCode::NO_CONTENT);
    };

    if let Some(tracker) = state
        .ctx
        .addons
        .media_tracker_for(addon_id)
    {
        let _ = tracker
            .disconnect(&existing.credentials, &tctx(&state))
            .await;
    }
    UserMediaTracker::delete(&state.ctx.db, existing.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SetFiltersBody {
    #[serde(rename = "eventFilters")]
    pub event_filters: Vec<MediaTrackerEventKindDto>,
}

/// Changes what gets synced without touching the stored credentials —
/// matches `UserMediaTracker::set_event_filters`'s own contract.
#[post("/media-trackers/{addon_id}/filters")]
pub async fn set_event_filters(
    State(state): State<AppState>,
    session: AuthSession,
    Path(addon_id): Path<Uuid>,
    Json(body): Json<SetFiltersBody>,
) -> Result<impl IntoResponse> {
    let existing =
        UserMediaTracker::get_for_user_and_addon(&state.ctx.db, session.user.id, addon_id)
            .await?
            .ok_or_else(|| {
                ApiError::builder()
                    .status(StatusCode::NOT_FOUND)
                    .title("media-tracker")
                    .detail("not connected")
                    .build()
            })?;
    let filters: Vec<MediaTrackerEventKind> = body
        .event_filters
        .into_iter()
        .map(event_kind_from_dto)
        .collect();
    UserMediaTracker::set_event_filters(&state.ctx.db, existing.id, &filters).await?;
    Ok(StatusCode::NO_CONTENT)
}
