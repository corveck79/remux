//! Reconciles watch history with every connected media tracker (Trakt today,
//! any future `MediaTrackerAddon` provider tomorrow), in both directions:
//!
//! - **Pull**: whatever the provider already has marked watched (from other
//!   clients, or from before the user connected) gets marked played locally.
//! - **Push**: whatever is marked played locally but missing from the
//!   provider's history gets synced up.
//!
//! Ongoing activity does not need this task at all — `MediaTrackerSubscriber`
//! (`services/media_tracker.rs`) already pushes every play/pause/stop/mark/
//! rating event live, the moment it happens. This task only backfills the
//! gap: history that predates the connection, or that arrived through a
//! provider client Remux never saw an event from.
//!
//! Runs once per connected `UserMediaTracker` row — each user's own history
//! syncs only against their own connection, never anyone else's.

use anyhow::Result;
use async_trait::async_trait;
use std::{collections::HashSet, sync::Arc};
use tracing::{debug, info, warn};

use super::{ProgressReporter, Task, TaskCategory, TaskService};
use crate::{
    AppContext,
    addons::media_tracker::{MediaTrackerCtx, MediaTrackerEvent, MediaTrackerEventKind},
    db,
    services::media_tracker::resolve_target,
};

pub struct MediaTrackerSyncTask;

#[async_trait]
impl Task for MediaTrackerSyncTask {
    fn key(&self) -> &str {
        "MediaTrackerSync"
    }

    fn name(&self) -> &str {
        "Sync Media Trackers"
    }

    fn description(&self) -> &str {
        "Reconciles watch history with connected media trackers (e.g. Trakt): \
         pulls in what the provider already has marked watched, and pushes up \
         what's marked played locally but missing there. Ongoing activity \
         syncs live on its own — this only backfills the gap."
    }
    fn short_description(&self) -> &str {
        "Two-way watch-history sync with connected media trackers"
    }
    fn category(&self) -> TaskCategory {
        TaskCategory::Users
    }

    async fn run(
        &self,
        ctx: AppContext,
        _tasks: Arc<TaskService>,
        progress: ProgressReporter,
    ) -> Result<()> {
        // Every enabled addon that can track, regardless of provider — a
        // second provider (Yamtrack, say) needs no changes here.
        let trackers: Vec<(uuid::Uuid, Arc<dyn crate::addons::media_tracker::MediaTrackerAddon>)> =
            ctx.addons
                .list()
                .iter()
                .filter(|r| r.row.enabled)
                .filter_map(|r| {
                    r.caps
                        .media_tracker
                        .clone()
                        .map(|t| (r.row.id, t))
                })
                .collect();

        if trackers.is_empty() {
            info!("no enabled media-tracker addons, nothing to sync");
            progress.set(100.0);
            return Ok(());
        }

        // One connection row per (user, addon) — every connected pair gets
        // its own pull+push pass, so users never see each other's history.
        let mut jobs: Vec<(db::UserMediaTracker, Arc<dyn crate::addons::media_tracker::MediaTrackerAddon>)> =
            Vec::new();
        for (addon_id, tracker) in &trackers {
            let caps = tracker.capabilities();
            let connections = db::UserMediaTracker::list_for_addon(&ctx.db, *addon_id)
                .await
                .unwrap_or_default();
            for conn in connections {
                if conn.status != db::MediaTrackerStatus::Connected {
                    continue;
                }
                if !caps.history_import && !conn.wants(MediaTrackerEventKind::MarkPlayed) {
                    // Neither direction has anything to do for this provider.
                    continue;
                }
                jobs.push((conn, tracker.clone()));
            }
        }

        if jobs.is_empty() {
            info!("no connected media-tracker users, nothing to sync");
            progress.set(100.0);
            return Ok(());
        }

        let total = jobs.len();
        let mut pulled_total = 0u32;
        let mut pushed_total = 0u32;

        for (i, (conn, tracker)) in jobs.into_iter().enumerate() {
            let tctx = MediaTrackerCtx {
                config: Arc::new(
                    ctx.config
                        .clone(),
                ),
            };
            let Some(user) = db::User::get_by_id(&ctx.db, &conn.user_id)
                .await
                .ok()
                .flatten()
            else {
                warn!(user_id = %conn.user_id, "media tracker connection has no matching user, skipping");
                progress.report(i + 1, total);
                continue;
            };

            // --- pull: provider -> local ---
            let caps = tracker.capabilities();
            let mut remote_ids: HashSet<(String, Option<i64>, Option<i64>)> = HashSet::new();
            if caps.history_import {
                match tracker
                    .import_history(&conn.credentials, &tctx)
                    .await
                {
                    Ok(watches) => {
                        debug!(user = %user.username, count = watches.len(), "provider history fetched");
                        for watch in watches {
                            // Track what the provider already has, so the push
                            // pass below doesn't immediately re-send it.
                            if let Some(key) = external_key(&watch.ids) {
                                remote_ids.insert((
                                    key,
                                    watch.season,
                                    watch.episode,
                                ));
                            }
                            if !watch.watched {
                                continue;
                            }
                            match resolve_local_media(&ctx, &watch).await {
                                Ok(Some(media)) => {
                                    let update = remux_sdks::remux::UpdateUserItemDataDto {
                                        played: Some(true),
                                        last_played_date: watch
                                            .watched_at
                                            .map(|dt| dt.and_utc()),
                                        ..Default::default()
                                    };
                                    if let Err(e) = db::UserMediaState::apply_update(
                                        &ctx.db, &user, &media, &update,
                                    )
                                    .await
                                    {
                                        warn!(user = %user.username, title = %media.title, error = %e, "failed to apply pulled watch state");
                                    } else {
                                        pulled_total += 1;
                                    }
                                }
                                Ok(None) => {
                                    debug!(?watch.ids, "provider history item not found locally, skipping");
                                }
                                Err(e) => {
                                    warn!(error = %e, "failed to resolve provider history item locally");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(user = %user.username, error = %e, "provider history import failed");
                    }
                }
            }

            // --- push: local -> provider ---
            if conn.wants(MediaTrackerEventKind::MarkPlayed) {
                let played = db::UserMediaState::get_by_filter(
                    &ctx.db,
                    &db::UserMediaStateFilter {
                        user_id: Some(user.id),
                        played: Some(true),
                        ..Default::default()
                    },
                )
                .await
                .map(|r| r.records)
                .unwrap_or_default();

                for state in played {
                    let Some(mut media) = db::Media::get_by_id(&ctx.db, &state.media_id)
                        .await
                        .ok()
                        .flatten()
                    else {
                        continue;
                    };
                    let Ok(Some(target)) = resolve_target(&ctx, &mut media).await else {
                        continue;
                    };
                    let key = target_key(&target);
                    if let Some(key) = &key {
                        if remote_ids.contains(key) {
                            // Provider already has this — the pull pass above
                            // just fetched it, or an earlier live event already
                            // pushed it. Sending it again would add a duplicate
                            // "watch" on the provider's side.
                            continue;
                        }
                    }
                    match tracker
                        .on_event(
                            &MediaTrackerEvent::MarkPlayed,
                            &target,
                            &conn.credentials,
                            &tctx,
                        )
                        .await
                    {
                        Ok(()) => pushed_total += 1,
                        Err(e) if e.is_retryable() => {
                            debug!(title = %media.title, error = %e, "push deferred (retryable)");
                        }
                        Err(e) => {
                            warn!(title = %media.title, error = %e, "failed to push watch state to provider");
                        }
                    }
                }
            }

            let _ = db::UserMediaTracker::mark_success(&ctx.db, conn.id).await;
            progress.report(i + 1, total);
        }

        info!(pulled = pulled_total, pushed = pushed_total, "media tracker sync complete");
        progress.set(100.0);
        Ok(())
    }
}

/// A dedup/lookup key from an external-id set: prefers imdb, falls back to
/// tmdb/tvdb — mirrors the preference order used elsewhere for matching.
fn external_key(ids: &db::ExternalIds) -> Option<String> {
    ids.imdb
        .as_ref()
        .map(|s| format!("imdb:{s}"))
        .or_else(|| {
            ids.tmdb
                .map(|id| format!("tmdb:{id}"))
        })
        .or_else(|| {
            ids.tvdb
                .map(|id| format!("tvdb:{id}"))
        })
}

/// Same key shape as `external_key`, built from an already-resolved
/// `MediaTrackerTarget` (movie's own ids, or the episode's series' ids).
fn target_key(
    target: &crate::addons::media_tracker::MediaTrackerTarget,
) -> Option<(String, Option<i64>, Option<i64>)> {
    let ids = match &target.series {
        Some(series) => &series.ids,
        None => &target.ids,
    };
    external_key(ids).map(|k| (k, target.season, target.episode))
}

/// Resolves a provider's `RemoteWatch` to a local `db::Media` row, if one
/// exists. Movies/series match directly on external ids; episodes resolve
/// the series first, then look up the (season, episode) position under it —
/// same two-step approach `JellyfinImportTask` uses for the same reason
/// (Trakt reports episodes by series identity + position, not their own id).
async fn resolve_local_media(
    ctx: &AppContext,
    watch: &crate::addons::media_tracker::RemoteWatch,
) -> Result<Option<db::Media>> {
    let is_episode = watch.season.is_some() && watch.episode.is_some();
    let probe_kind = if is_episode {
        db::MediaKind::Series
    } else {
        db::MediaKind::Movie
    };
    let probe = db::Media {
        kind: probe_kind,
        external_ids: watch.ids.clone(),
        ..Default::default()
    };
    let Some(matched_id) = db::Media::find_existing_id_by_ext(&ctx.db, &probe).await else {
        return Ok(None);
    };

    if !is_episode {
        return Ok(db::Media::get_by_id(&ctx.db, &matched_id).await?);
    }

    let (season, episode) = (
        watch
            .season
            .unwrap(),
        watch
            .episode
            .unwrap(),
    );
    let episode_id: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM media WHERE kind = 'episode' AND grandparent_id = ? \
         AND parent_idx = ? AND idx = ? LIMIT 1",
    )
    .bind(matched_id)
    .bind(season)
    .bind(episode)
    .fetch_optional(&ctx.db)
    .await
    .unwrap_or(None);

    match episode_id {
        Some(id) => Ok(db::Media::get_by_id(&ctx.db, &id).await?),
        None => Ok(None),
    }
}
