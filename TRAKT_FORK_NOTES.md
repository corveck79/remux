# Trakt integration — fork notes

This fork of [lostb1t/remux](https://github.com/lostb1t/remux) adds a native
Trakt.tv integration: connecting a personal Trakt account and scrobbling
watch activity, marking history, and syncing ratings, directly from the
Remux dashboard — no separate Stremio-protocol addon involved.

## Why

Remux already ships a generic `MediaTrackerAddon` framework
(`crates/remux-server/src/addons/media_tracker.rs`) intended for services
like Trakt or Yamtrack, plus a complete per-user DB layer
(`db/user_media_tracker.rs`) and event dispatcher
(`services/media_tracker.rs`) — but no concrete provider implemented it yet,
and there was no way to connect one from the UI. This fork fills in the
missing piece for Trakt specifically.

## What changed

- **`crates/remux-sdks/src/trakt/`** — extended with the OAuth
  device-code flow (`/oauth/device/code`, `/oauth/device/token`,
  `/oauth/token` refresh) and the scrobble/sync endpoints
  (`/scrobble/{start,pause,stop}`, `/sync/history`, `/sync/ratings`,
  `/sync/watchlist`). This file previously existed with only the
  unauthenticated popularity/stats endpoints and, as far as we could tell,
  was not wired into the crate's module tree at all (`pub mod trakt;` was
  missing from `remux-sdks/src/lib.rs`) — added that too.
- **`crates/remux-server/src/addons/trakt.rs`** *(new)* — `TraktPreset` +
  `TraktMediaTracker`, implementing `MediaTrackerAddon`: device-code
  login, playback-start/progress/stop → Trakt scrobble, mark
  played/unplayed → Trakt history, ratings → Trakt ratings, plus a
  best-effort history import for newly connected accounts.
- **`crates/remux-server/src/api/media_trackers.rs`** *(new)* — REST
  endpoints (`GET /media-trackers`, `POST .../connect/device`,
  `POST .../connect/device/poll`, `DELETE /media-trackers/{id}`,
  `POST .../filters`), generic over any addon implementing
  `MediaTrackerAddon`, not Trakt-specific.
- **`crates/remux-sdks/src/remux/mod.rs`** — shared DTOs and dashboard
  `Endpoint` structs for the above (`MediaTrackerProviderDto`,
  `BeginDeviceAuth`, `PollDeviceAuth`, `DisconnectMediaTracker`,
  `SetMediaTrackerFilters`), so the server and dashboard can't drift on
  field names.
- **`crates/remux-dashboard/src/pages/trackers.rs`** *(new)* — a
  Settings → Media Trackers page: shows every media-tracker-capable addon,
  a Connect button that walks through the device-code flow (code +
  verification URL, polls until approved), and Disconnect. Wired into
  `router.rs` (`SettingsTrackersRoute`) and the sidebar in `layout.rs`.
- **`crates/remux-server/src/lib.rs`** — two new operator-level `Config`
  fields, `trakt_client_id` / `trakt_client_secret`
  (`TRAKT_CLIENT_ID` / `TRAKT_CLIENT_SECRET` env vars). Device-code flow
  needs no redirect URI, so one registered Trakt application's
  client_id/secret can authorize any number of independent users — the
  addon takes no per-user or per-addon config of its own.

## Compatibility

Everything above is additive: no existing route, DB column, or addon
changed behavior. `TraktPreset::from_cfg` only errors if the addon is
enabled without `TRAKT_CLIENT_ID`/`TRAKT_CLIENT_SECRET` set, same failure
shape as any other misconfigured addon.

## License

Remux is AGPL-3.0. This fork's source is published here in full per §13;
no separate distribution channel is used.
