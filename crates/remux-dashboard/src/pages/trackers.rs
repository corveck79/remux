//! Settings → Media Trackers: connect the signed-in user's own account to
//! any enabled `MediaTrackerAddon` (currently just Trakt) via device-code
//! login, and manage the resulting connection. Mirrors the
//! request/poll/render shape of `JellyfinImportCard` in `settings.rs`.

use crate::{
    components::{Card, ErrorAlert, LoadingText, SuccessAlert},
    state::AppState,
};
use dioxus::prelude::*;
use remux_sdks::remux::{
    BeginDeviceAuth, DevicePollStatusDto, DisconnectMediaTracker, ListMediaTrackers,
    MediaTrackerProviderDto, MediaTrackerStatusDto, PollDeviceAuth,
};

#[component]
pub fn MediaTrackersCard(app_state: AppState) -> Element {
    let mut loading = use_signal(|| true);
    let mut load_error = use_signal(|| Option::<String>::None);
    let mut providers: Signal<Vec<MediaTrackerProviderDto>> = use_signal(Vec::new);
    let mut refresh_tick = use_signal(|| 0_u32);

    let app_state_load = app_state.clone();
    use_effect(move || {
        let _ = refresh_tick.read();
        let client = app_state_load.clone();
        loading.set(true);
        spawn(async move {
            match client
                .execute(ListMediaTrackers)
                .await
            {
                Ok(p) => {
                    providers.set(p);
                    load_error.set(None);
                }
                Err(e) => load_error.set(Some(e.user_message())),
            }
            loading.set(false);
        });
    });

    rsx! {
        Card { title: "Media Trackers",
            p { class: "field-hint", style: "margin-bottom: 14px",
                "Sync watch activity, history and ratings with an external service. Each user connects their own account."
            }
            if *loading.read() {
                LoadingText {}
            } else {
                if let Some(err) = load_error.read().as_ref() {
                    ErrorAlert { message: err.clone() }
                }
                if providers.read().is_empty() {
                    p { class: "field-hint", "No media-tracker addon is enabled on this server." }
                } else {
                    div { style: "display:flex;flex-direction:column;gap:16px",
                        for provider in providers.read().iter().cloned() {
                            TrackerProviderRow {
                                app_state: app_state.clone(),
                                provider,
                                on_changed: move |_| {
                                    let next = refresh_tick.peek().wrapping_add(1);
                                    refresh_tick.set(next);
                                },
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn TrackerProviderRow(
    app_state: AppState,
    provider: MediaTrackerProviderDto,
    on_changed: EventHandler<()>,
) -> Element {
    let mut connecting = use_signal(|| false);
    let mut connect_error = use_signal(|| Option::<String>::None);
    let mut device_code: Signal<Option<(String, String)>> = use_signal(|| None); // (user_code, verification_url)
    let mut disconnecting = use_signal(|| false);

    let addon_id = provider.addon_id;
    let is_connected = provider.connected;
    let status = provider.status;

    let app_state_connect = app_state.clone();
    let on_connect = move |_| {
        let client = app_state_connect.clone();
        connecting.set(true);
        connect_error.set(None);
        device_code.set(None);
        spawn(async move {
            let start = match client
                .execute(BeginDeviceAuth { addon_id })
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    connect_error.set(Some(e.user_message()));
                    connecting.set(false);
                    return;
                }
            };
            device_code.set(Some((
                start.user_code.clone(),
                start.verification_url.clone(),
            )));

            let poll_token = start.poll_token.clone();
            let interval = std::time::Duration::from_secs(start.interval_secs.max(1));
            let deadline = std::time::Duration::from_secs(start.expires_in_secs.max(1));
            let mut waited = std::time::Duration::ZERO;

            loop {
                gloo_timers::future::sleep(interval).await;
                waited += interval;

                match client
                    .execute(PollDeviceAuth {
                        addon_id,
                        poll_token: poll_token.clone(),
                    })
                    .await
                {
                    Ok(result) => match result.status {
                        DevicePollStatusDto::Connected => {
                            device_code.set(None);
                            connecting.set(false);
                            on_changed.call(());
                            return;
                        }
                        DevicePollStatusDto::Denied => {
                            connect_error.set(Some(
                                "Login was denied or the code expired. Try again.".to_string(),
                            ));
                            device_code.set(None);
                            connecting.set(false);
                            return;
                        }
                        DevicePollStatusDto::Pending => {}
                    },
                    Err(e) => {
                        connect_error.set(Some(e.user_message()));
                        device_code.set(None);
                        connecting.set(false);
                        return;
                    }
                }

                if waited >= deadline {
                    connect_error.set(Some("Code expired. Try again.".to_string()));
                    device_code.set(None);
                    connecting.set(false);
                    return;
                }
            }
        });
    };

    let app_state_disconnect = app_state.clone();
    let on_disconnect = move |_| {
        let client = app_state_disconnect.clone();
        disconnecting.set(true);
        spawn(async move {
            if client
                .execute(DisconnectMediaTracker { addon_id })
                .await
                .is_ok()
            {
                on_changed.call(());
            }
            disconnecting.set(false);
        });
    };

    rsx! {
        div {
            style: "border:1px solid var(--border-color, #333);border-radius:8px;padding:14px;display:flex;flex-direction:column;gap:10px",
            div { style: "display:flex;justify-content:space-between;align-items:center",
                div {
                    div { style: "font-weight:600", "{provider.display_name}" }
                    if is_connected {
                        div { class: "field-hint",
                            match status {
                                Some(MediaTrackerStatusDto::Connected) => "Connected".to_string(),
                                Some(MediaTrackerStatusDto::AuthExpired) => "Reconnect needed — the stored login expired.".to_string(),
                                Some(MediaTrackerStatusDto::Error) => "Connected, last sync failed.".to_string(),
                                _ => "Connected".to_string(),
                            }
                        }
                    } else {
                        div { class: "field-hint", "Not connected" }
                    }
                }
                if is_connected {
                    button {
                        r#type: "button",
                        class: "btn btn-secondary",
                        disabled: *disconnecting.read(),
                        onclick: on_disconnect,
                        if *disconnecting.read() { "Disconnecting…" } else { "Disconnect" }
                    }
                } else {
                    button {
                        r#type: "button",
                        class: "btn btn-primary",
                        disabled: *connecting.read(),
                        onclick: on_connect,
                        if *connecting.read() { "Connecting…" } else { "Connect" }
                    }
                }
            }

            if let Some((code, url)) = device_code.read().as_ref() {
                div {
                    style: "background:var(--surface-alt,#1a1a1a);border-radius:6px;padding:12px;display:flex;flex-direction:column;gap:6px",
                    p { style: "margin:0",
                        "Go to "
                        a { href: "{url}", target: "_blank", rel: "noopener noreferrer", "{url}" }
                        " and enter this code:"
                    }
                    p { style: "font-size:1.6em;font-weight:700;letter-spacing:0.15em;margin:4px 0", "{code}" }
                    p { class: "field-hint", "Waiting for confirmation…" }
                }
            }

            if let Some(err) = connect_error.read().as_ref() {
                ErrorAlert { message: err.clone() }
            }
        }
    }
}
