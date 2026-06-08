//! Dashboard view: service status cards, the runtime facts panel, stack
//! controls, and the live log tail.

use dioxus::prelude::*;
use dioxus_i18n::t;

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
