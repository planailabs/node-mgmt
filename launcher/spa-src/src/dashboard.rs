//! Dashboard view: service status cards, the runtime facts panel, stack
//! controls, and the live log tail.

use dioxus::prelude::*;
use dioxus_i18n::t;
use serde_json::json;

use plan_ai_design::{
    Button, ButtonSize, ButtonVariant, Card, Dot, HelpText, Kicker, PageHero, Pill, PillVariant,
};

use crate::api;
use crate::app::{AppState, Tab};

fn pill_variant(state: &str) -> PillVariant {
    match state {
        "ready" => PillVariant::Ok,
        "starting" => PillVariant::Warn,
        "error" => PillVariant::Bad,
        _ => PillVariant::Muted,
    }
}

/// Localised label for a service state (the wire value stays english).
fn state_label(state: &str) -> String {
    match state {
        "ready" => t!("state-ready"),
        "starting" => t!("state-starting"),
        "stopped" => t!("state-stopped"),
        "error" => t!("state-error"),
        other => other.to_string(),
    }
}

fn fact<'a>(info: &'a serde_json::Value, key: &str) -> String {
    info.get(key)
        .and_then(|v| v.as_str().map(String::from).or_else(|| v.as_u64().map(|n| n.to_string())))
        .unwrap_or_else(|| "—".into())
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
            let _ = api::post_action(&path).await;
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
                            Dot { variant: pill_variant(&s.state) }
                            div {
                                div { class: "h-card", "{s.name}" }
                                div { class: "help-xs",
                                    {if s.id == "ollama" { t!("svc-ollama-sub") } else { t!("svc-webui-sub") }}
                                }
                            }
                        }
                        div { class: "flex items-center gap-2",
                            Pill { variant: pill_variant(&s.state), {state_label(&s.state)} }
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
                        Fact { label: t!("fact-ollama-port"), value: fact(info, "ollama_port") }
                        Fact { label: t!("fact-webui-port"), value: fact(info, "webui_port") }
                        Fact { label: t!("fact-models"), value: fact(info, "models_dir"), mono_muted: true }
                        Fact { label: t!("fact-data"), value: fact(info, "data_dir"), mono_muted: true }
                    }
                    div { class: "mt-4 pt-3 border-t border-line",
                        div { class: "label", {t!("accel-label")} }
                        div { class: "flex items-baseline gap-2",
                            span { class: "font-mono text-fg-strong",
                                {info.get("accel").and_then(|a| a.get("flavour")).and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| t!("accel-bundled"))}
                            }
                            span { class: "help-xs td-muted",
                                {info.get("accel").and_then(|a| a.get("reason")).and_then(|v| v.as_str()).map(|r| format!("— {r}")).unwrap_or_default()}
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

/// Manual update check + apply + per-platform keep/prune (driven by /api/update/*
/// and /api/platforms). Previewable against the mock server via `make ui`.
#[allow(non_snake_case)]
fn UpdatesCard() -> Element {
    let mut status = use_signal(|| json!({ "state": "idle" }));
    let mut kept = use_signal(Vec::<String>::new);
    let mut available = use_signal(Vec::<String>::new);

    use_future(move || async move {
        loop {
            if let Ok(v) = api::get_json("/api/update/status").await {
                status.set(v);
            }
            gloo_timers::future::TimeoutFuture::new(2000).await;
        }
    });
    use_future(move || async move {
        if let Ok(v) = api::get_json("/api/platforms").await {
            let arr = |k: &str| {
                v.get(k)
                    .and_then(|a| a.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>())
                    .unwrap_or_default()
            };
            kept.set(arr("kept"));
            available.set(arr("available"));
        }
    });

    let s = status.read();
    let state = s.get("state").and_then(|v| v.as_str()).unwrap_or("idle").to_string();
    let done = s.get("done").and_then(|v| v.as_u64()).unwrap_or(0);
    let total = s.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    let version = s.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
    drop(s);
    let pct: i64 = if total > 0 { (done * 100 / total) as i64 } else { 0 };
    let busy = matches!(state.as_str(), "checking" | "downloading" | "applying");
    let kept_now = kept.read().clone();
    let avail = available.read().clone();

    rsx! {
        Card { class: "card-pad space-y-3",
            div { class: "flex items-center justify-between",
                Kicker { {t!("updates-title")} }
                Button {
                    size: ButtonSize::Xs,
                    variant: ButtonVariant::Secondary,
                    disabled: busy,
                    onclick: move |_| { spawn(async move { let _ = api::post_json("/api/update/check", json!({})).await; }); },
                    {t!("btn-check-updates")}
                }
            }
            div { class: "text-sm",
                {match state.as_str() {
                    "downloading" => rsx! { span { class: "text-warn-strong", {t!("upd-downloading", pct: pct)} } },
                    "checking" => rsx! { span { class: "td-muted", {t!("upd-checking")} } },
                    "applying" => rsx! { span { class: "text-warn-strong", {t!("upd-applying")} } },
                    "failed" => rsx! { span { class: "text-warn-strong", {t!("upd-failed")} } },
                    "ready" => rsx! {
                        div { class: "flex items-center gap-3",
                            span { class: "text-success font-medium", {t!("upd-ready", version: version.clone())} }
                            Button {
                                size: ButtonSize::Sm,
                                variant: ButtonVariant::Accent,
                                onclick: move |_| { spawn(async move { let _ = api::post_json("/api/update/apply", json!({})).await; }); },
                                {t!("btn-apply-update")}
                            }
                        }
                    },
                    _ => rsx! { span { class: "td-muted", {t!("upd-idle")} } },
                }}
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
                    Button {
                        size: ButtonSize::Xs,
                        variant: ButtonVariant::Primary,
                        onclick: move |_| {
                            let k = kept.read().clone();
                            spawn(async move { let _ = api::post_json("/api/platforms", json!({ "platforms": k })).await; });
                        },
                        {t!("btn-save")}
                    }
                }
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
