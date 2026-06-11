//! Dashboard view: service status cards, the runtime facts panel, stack
//! controls, and the live log tail.

use dioxus::prelude::*;
use dioxus_i18n::t;
use serde_json::json;

use plan_ai_design::{
    Button, ButtonSize, ButtonVariant, Card, Dot, HelpText, Kicker, PageHero, Pill, PillVariant,
};

use crate::api::{self, Platforms, ServiceState, UpdateState, UpdateStatus};
use crate::app::{AppState, Tab};

fn pill_variant(state: ServiceState) -> PillVariant {
    match state {
        ServiceState::Ready => PillVariant::Ok,
        ServiceState::Starting => PillVariant::Warn,
        ServiceState::Error => PillVariant::Bad,
        ServiceState::Stopped => PillVariant::Muted,
    }
}

/// Localised label for a service state (the wire value stays english).
fn state_label(state: ServiceState) -> String {
    match state {
        ServiceState::Ready => t!("state-ready"),
        ServiceState::Starting => t!("state-starting"),
        ServiceState::Stopped => t!("state-stopped"),
        ServiceState::Error => t!("state-error"),
    }
}

#[allow(non_snake_case)]
pub fn Dashboard() -> Element {
    let state = use_context::<AppState>();
    let services = state.services.read().clone();
    let info = state.info.read().clone();
    let logs = state.logs.read().clone();
    let webui_ready = state.ready("webui");

    // Keep the log tail pinned to the bottom as new lines stream in.
    use_effect(move || {
        let _ = state.logs.read().len();
        document::eval("var e=document.getElementById('logs'); if(e){e.scrollTop=e.scrollHeight;}");
    });

    let control = move |path: String| {
        spawn(async move {
            let _ = api::command(&path).await;
        });
    };

    rsx! {
        main { class: "h-page flex-1 overflow-auto p-6 space-y-6",
            PageHero {
                title: rsx! {
                    {t!("dash-title-lead")}
                    " "
                    span { class: "text-fg-muted", {t!("dash-title-tail")} }
                },
            }

            // service status cards
            section { class: "grid grid-cols-1 md:grid-cols-2 gap-4",
                for s in services.iter().cloned() {
                    Card { class: "card-pad flex items-center justify-between",
                        div { class: "flex items-center gap-3",
                            Dot { variant: pill_variant(s.state) }
                            div {
                                div { class: "h-card", "{s.name}" }
                                div { class: "help-xs",
                                    {if s.id == "ollama" { t!("svc-ollama-sub") } else { t!("svc-webui-sub") }}
                                }
                            }
                        }
                        div { class: "flex items-center gap-2",
                            Pill { variant: pill_variant(s.state), {state_label(s.state)} }
                            Button {
                                size: ButtonSize::Xs,
                                variant: ButtonVariant::Secondary,
                                onclick: {
                                    let id = s.id.clone();
                                    move |_| control(format!("/api/services/{id}/restart"))
                                },
                                {t!("btn-restart")}
                            }
                        }
                    }
                }
            }

            // Open-WebUI readiness: orange while it starts, green + "open" (jumps to
            // the WebUI tab) once ready.
            Card { class: "card-pad flex items-center justify-center gap-3 text-center",
                if webui_ready {
                    span { class: "text-success font-medium", {t!("webui-ready")} }
                    Button {
                        size: ButtonSize::Sm,
                        variant: ButtonVariant::Accent,
                        onclick: move |_| {
                            let mut t = state.tab;
                            t.set(Tab::WebUi);
                        },
                        {t!("btn-open")}
                    }
                } else {
                    span { class: "text-warn-strong font-medium", {t!("webui-starting")} }
                }
            }

            UpdatesCard {}

            // runtime facts
            if let Some(info) = info.as_ref() {
                Card { class: "card-pad",
                    div { class: "grid grid-cols-2 md:grid-cols-4 gap-4 text-sm",
                        Fact { label: t!("fact-ollama-port"), value: info.ollama_port.to_string() }
                        Fact { label: t!("fact-webui-port"), value: info.webui_port.to_string() }
                        Fact { label: t!("fact-models"), value: info.models_dir.clone(), mono_muted: true }
                        Fact { label: t!("fact-data"), value: info.data_dir.clone(), mono_muted: true }
                    }
                    div { class: "mt-4 pt-3 border-t border-line",
                        div { class: "label", {t!("accel-label")} }
                        div { class: "flex items-baseline gap-2",
                            span { class: "font-mono text-fg-strong",
                                {info.accel.flavour.clone().unwrap_or_else(|| t!("accel-bundled"))}
                            }
                            span { class: "help-xs td-muted",
                                {info.accel.reason.as_ref().map(|r| format!("— {r}")).unwrap_or_default()}
                            }
                        }
                    }
                }
            }

            // controls
            section { class: "flex gap-2",
                Button {
                    size: ButtonSize::Md,
                    variant: ButtonVariant::Primary,
                    onclick: move |_| control("/api/services/all/start".into()),
                    {t!("btn-start-all")}
                }
                Button {
                    size: ButtonSize::Md,
                    variant: ButtonVariant::Secondary,
                    onclick: move |_| control("/api/services/all/stop".into()),
                    {t!("btn-stop-all")}
                }
                Button {
                    size: ButtonSize::Md,
                    variant: ButtonVariant::Accent,
                    disabled: !webui_ready,
                    onclick: move |_| {
                        let mut t = state.tab;
                        t.set(Tab::WebUi);
                    },
                    {t!("btn-open-webui")}
                }
            }

            // logs
            Card { class: "",
                div { class: "card-pad pb-2 flex items-center justify-between",
                    Kicker { {t!("logs-kicker")} }
                    Button {
                        size: ButtonSize::Xs,
                        variant: ButtonVariant::Ghost,
                        onclick: move |_| {
                            let mut l = state.logs;
                            l.set(String::new());
                        },
                        {t!("btn-clear")}
                    }
                }
                pre { id: "logs", class: "log-output", "{logs}" }
            }

            if services.is_empty() {
                HelpText { xs: true, {t!("waiting-supervisor")} }
            }
        }
    }
}

/// Human-readable byte size for the throughput indicator (1.5 GB, 640 MB, 12.3 MB).
fn fmt_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut f = n as f64;
    let mut i = 0usize;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else if f >= 100.0 {
        format!("{f:.0} {}", U[i])
    } else {
        format!("{f:.1} {}", U[i])
    }
}

/// Manual update check + apply + per-platform keep/prune (driven by /api/update/*
/// and /api/platforms). Previewable against the mock server via `make ui`.
#[allow(non_snake_case)]
fn UpdatesCard() -> Element {
    let mut status = use_signal(UpdateStatus::idle);
    let mut kept = use_signal(Vec::<String>::new);
    let mut available = use_signal(Vec::<String>::new);
    let mut features = use_signal(Vec::<String>::new);
    let mut available_features = use_signal(Vec::<String>::new);

    use_future(move || async move {
        loop {
            if let Ok(v) = api::get::<UpdateStatus>("/api/update/status").await {
                status.set(v);
            }
            gloo_timers::future::TimeoutFuture::new(2000).await;
        }
    });
    use_future(move || async move {
        if let Ok(p) = api::get::<Platforms>("/api/platforms").await {
            kept.set(p.kept);
            available.set(p.available);
            features.set(p.features);
            available_features.set(p.available_features);
        }
    });

    let s = status.read();
    let state = s.state;
    let done = s.done;
    let total = s.total;
    let done_bytes = s.done_bytes;
    let total_bytes = s.total_bytes;
    let rate_bps = s.rate_bps;
    let version = s.version.clone();
    drop(s);
    let pct: i64 = if total > 0 { (done * 100 / total) as i64 } else { 0 };
    let busy = matches!(state, UpdateState::Checking | UpdateState::Downloading | UpdateState::Applying);
    // Throughput indicator (download / copy speed) — shown while transferring.
    let transferring = matches!(state, UpdateState::Downloading | UpdateState::Applying);
    let throughput = if transferring && (rate_bps > 0 || total_bytes > 0) {
        let prog = if total_bytes > 0 {
            format!("{} / {}", fmt_bytes(done_bytes), fmt_bytes(total_bytes))
        } else {
            fmt_bytes(done_bytes)
        };
        if rate_bps > 0 {
            Some(format!("{} · {}/s", prog, fmt_bytes(rate_bps)))
        } else {
            Some(prog)
        }
    } else {
        None
    };
    let kept_now = kept.read().clone();
    let avail = available.read().clone();
    let feats_now = features.read().clone();
    let avail_feats = available_features.read().clone();

    rsx! {
        Card { class: "card-pad space-y-3",
            div { class: "flex items-center justify-between",
                Kicker { {t!("updates-title")} }
                Button {
                    size: ButtonSize::Xs,
                    variant: ButtonVariant::Secondary,
                    disabled: busy,
                    onclick: move |_| { spawn(async move { let _ = api::command("/api/update/check").await; }); },
                    {t!("btn-check-updates")}
                }
            }
            div { class: "text-sm",
                {match state {
                    UpdateState::Downloading => rsx! { span { class: "text-warn-strong", {t!("upd-downloading", pct: pct)} } },
                    UpdateState::Checking => rsx! { span { class: "td-muted", {t!("upd-checking")} } },
                    UpdateState::Applying => rsx! { span { class: "text-warn-strong", {t!("upd-applying")} } },
                    UpdateState::Failed => rsx! { span { class: "text-warn-strong", {t!("upd-failed")} } },
                    UpdateState::Ready => rsx! {
                        div { class: "flex items-center gap-3",
                            span { class: "text-success font-medium", {t!("upd-ready", version: version.clone())} }
                            Button {
                                size: ButtonSize::Sm,
                                variant: ButtonVariant::Accent,
                                onclick: move |_| { spawn(async move { let _ = api::command("/api/update/apply").await; }); },
                                {t!("btn-apply-update")}
                            }
                        }
                    },
                    UpdateState::Idle => rsx! { span { class: "td-muted", {t!("upd-idle")} } },
                }}
            }
            if let Some(tp) = throughput {
                div { class: "text-xs td-muted tabular-nums", "{tp}" }
            }
            div { class: "pt-2 border-t border-line space-y-2",
                div { class: "label", {t!("platforms-title")} }
                div { class: "flex items-center gap-2",
                    for p in avail.iter().cloned() {
                        {
                            let on = kept_now.contains(&p);
                            let pc = p.clone();
                            rsx! {
                                Button {
                                    size: ButtonSize::Xs,
                                    variant: if on { ButtonVariant::Secondary } else { ButtonVariant::Ghost },
                                    onclick: move |_| {
                                        let mut k = kept.write();
                                        if let Some(i) = k.iter().position(|x| x == &pc) { k.remove(i); } else { k.push(pc.clone()); }
                                    },
                                    "{p}"
                                }
                            }
                        }
                    }
                }
                div { class: "label", {t!("features-title")} }
                div { class: "flex items-center gap-2",
                    for f in avail_feats.iter().cloned() {
                        {
                            let on = feats_now.contains(&f);
                            let fc = f.clone();
                            rsx! {
                                Button {
                                    size: ButtonSize::Xs,
                                    variant: if on { ButtonVariant::Secondary } else { ButtonVariant::Ghost },
                                    onclick: move |_| {
                                        let mut x = features.write();
                                        if let Some(i) = x.iter().position(|v| v == &fc) { x.remove(i); } else { x.push(fc.clone()); }
                                    },
                                    "{f}"
                                }
                            }
                        }
                    }
                    Button {
                        size: ButtonSize::Xs,
                        variant: ButtonVariant::Primary,
                        onclick: move |_| {
                            let k = kept.read().clone();
                            let f = features.read().clone();
                            spawn(async move {
                                // Saving also kicks an update check server-side, so a
                                // re-added platform / newly enabled feature downloads
                                // right away.
                                let _ = api::command_json("/api/platforms", json!({ "platforms": k, "features": f })).await;
                            });
                        },
                        {t!("btn-save")}
                    }
                }
                HelpText { xs: true, {t!("feature-hint")} }
            }
        }
    }
}

#[component]
fn Fact(label: String, value: String, #[props(default)] mono_muted: bool) -> Element {
    let cls = if mono_muted { "font-mono td-muted truncate" } else { "font-mono text-fg-strong" };
    rsx! {
        div {
            div { class: "label", "{label}" }
            div { class: "{cls}", title: "{value}", "{value}" }
        }
    }
}
