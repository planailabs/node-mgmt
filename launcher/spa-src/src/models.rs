//! Models view: GPU-aware compatible models (llmfit) + one-click download into
//! ollama, plus the list of already-installed models. Proxied through the
//! launcher's /api/llmfit/* (same-origin) so there's no CORS or exposed port.

use std::collections::HashMap;

use dioxus::prelude::*;
use dioxus_i18n::t;
use serde_json::{json, Value};

use plan_ai_design::{Button, ButtonSize, ButtonVariant, Card, PageHero, Pill, PillVariant};

/// Localised label for a use-case option (the wire value stays english).
fn use_case_label(uc: &str) -> String {
    match uc {
        "general" => t!("uc-general"),
        "coding" => t!("uc-coding"),
        "reasoning" => t!("uc-reasoning"),
        "chat" => t!("uc-chat"),
        "multimodal" => t!("uc-multimodal"),
        "embedding" => t!("uc-embedding"),
        other => other.to_string(),
    }
}

use crate::api;

fn fit_variant(level: &str) -> PillVariant {
    match level {
        "perfect" | "good" => PillVariant::Ok,
        "marginal" | "tight" => PillVariant::Warn,
        _ => PillVariant::Muted,
    }
}

fn hardware_line(sys: &Value) -> String {
    let gpu = match sys.get("gpu_name").and_then(|v| v.as_str()) {
        Some(name) if !name.is_empty() => {
            let vram = sys.get("gpu_vram_gb").and_then(|v| v.as_f64()).map(|g| format!("{g} GB VRAM")).unwrap_or_else(|| "? GB VRAM".into());
            let backend = sys.get("backend").and_then(|v| v.as_str()).unwrap_or("");
            format!("{name} · {vram} · {backend}")
        }
        _ => {
            let backend = sys.get("backend").and_then(|v| v.as_str()).unwrap_or("");
            format!("CPU only · {backend}")
        }
    };
    let ram = sys.get("total_ram_gb").and_then(|v| v.as_f64()).map(|r| format!("{r:.1}")).unwrap_or_else(|| "?".into());
    let cpu = sys.get("cpu_name").and_then(|v| v.as_str()).unwrap_or("");
    format!("{gpu}  —  {ram} GB RAM  —  {cpu}")
}

fn model_meta(m: &Value) -> String {
    let params = m
        .get("parameter_count")
        .and_then(|v| v.as_str().map(String::from))
        .or_else(|| m.get("params_b").and_then(|v| v.as_f64()).map(|p| format!("{p}B")))
        .unwrap_or_default();
    let quant = m.get("best_quant").and_then(|v| v.as_str()).unwrap_or("");
    let mode = m.get("run_mode_label").and_then(|v| v.as_str()).unwrap_or("");
    let tps = m.get("estimated_tps").and_then(|v| v.as_f64()).map(|t| format!("~{} tok/s", t.round() as i64)).unwrap_or_default();
    [params.as_str(), quant, mode, tps.as_str()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" · ")
}

#[allow(non_snake_case)]
pub fn Models() -> Element {
    let mut use_case = use_signal(|| "general".to_string());
    let mut min_fit = use_signal(|| "good".to_string());
    let mut refresh = use_signal(|| 0u32);
    let installed_refresh = use_signal(|| 0u32);
    let progress = use_signal(HashMap::<String, String>::new);

    let models_res = use_resource(move || async move {
        let uc = use_case();
        let mf = min_fit();
        let _ = refresh();
        let mut path = format!("/api/llmfit/models?limit=12&use_case={uc}");
        if !mf.is_empty() {
            path.push_str(&format!("&min_fit={mf}"));
        }
        api::get_json(&path).await
    });

    let installed_res = use_resource(move || async move {
        let _ = installed_refresh();
        api::get_json("/api/llmfit/installed").await
    });

    let res = models_res.read();
    let installed = installed_res.read();

    rsx! {
        main { class: "h-page flex-1 overflow-auto p-6 space-y-6",
            PageHero {
                title: rsx! {
                    {t!("models-title-lead")}
                    " "
                    span { class: "text-fg-muted", {t!("models-title-tail")} }
                },
            }

            // detected hardware
            if let Some(Ok(data)) = res.as_ref() {
                if let Some(sys) = data.get("system") {
                    Card { class: "card-pad",
                        div { class: "label", {t!("detected-hardware")} " " span { class: "help-xs td-muted", {t!("llmfit-tag")} } }
                        div { class: "font-mono text-sm text-fg-strong mt-1", "{hardware_line(sys)}" }
                    }
                }
            }

            // controls
            section { class: "flex items-end gap-3 flex-wrap",
                label { class: "text-sm",
                    div { class: "label", {t!("use-case")} }
                    select {
                        class: "font-mono text-sm",
                        value: "{use_case}",
                        oninput: move |e| use_case.set(e.value()),
                        for opt in ["general", "coding", "reasoning", "chat", "multimodal", "embedding"] {
                            option { value: "{opt}", {use_case_label(opt)} }
                        }
                    }
                }
                label { class: "text-sm",
                    div { class: "label", {t!("min-fit")} }
                    select {
                        class: "font-mono text-sm",
                        value: "{min_fit}",
                        oninput: move |e| min_fit.set(e.value()),
                        option { value: "", {t!("fit-any")} }
                        option { value: "marginal", {t!("fit-marginal")} }
                        option { value: "good", {t!("fit-good")} }
                        option { value: "perfect", {t!("fit-perfect")} }
                    }
                }
                Button {
                    size: ButtonSize::Sm,
                    variant: ButtonVariant::Secondary,
                    onclick: move |_| refresh += 1,
                    {t!("btn-refresh")}
                }
                span { class: "help-xs td-muted",
                    match res.as_ref() {
                        Some(Ok(d)) => t!("models-shown",
                            returned: d.get("returned_models").and_then(|v| v.as_u64()).unwrap_or(0),
                            total: d.get("total_models").and_then(|v| v.as_u64()).unwrap_or(0)),
                        Some(Err(e)) => e.clone(),
                        None => t!("loading"),
                    }
                }
            }

            // compatible models
            Card { class: "",
                div { class: "card-pad pb-2 kicker", {t!("compatible-models")} }
                div { class: "divide-y divide-line",
                    match res.as_ref() {
                        Some(Ok(d)) => {
                            let models = d.get("models").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                            if models.is_empty() {
                                rsx! { div { class: "card-pad td-muted text-sm", {t!("no-compatible-models")} } }
                            } else {
                                rsx! {
                                    for m in models {
                                        ModelRow { model: m, progress, installed_refresh }
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => rsx! { div { class: "card-pad td-muted text-sm", "{e}" } },
                        None => rsx! { div { class: "card-pad td-muted text-sm", {t!("loading")} } },
                    }
                }
            }

            // installed
            Card { class: "",
                div { class: "card-pad pb-2 kicker", {t!("installed-ollama")} }
                div { class: "card-pad pt-0 font-mono text-sm td-muted",
                    match installed.as_ref() {
                        Some(Ok(d)) => {
                            let names = installed_names(d);
                            if names.is_empty() { t!("none-yet") } else { names.join("   ·   ") }
                        }
                        Some(Err(e)) => e.clone(),
                        None => "—".to_string(),
                    }
                }
            }
        }
    }
}

fn installed_names(d: &Value) -> Vec<String> {
    let arr = if let Some(a) = d.as_array() {
        a.clone()
    } else if let Some(a) = d.get("installed").and_then(|v| v.as_array()) {
        a.clone()
    } else if let Some(a) = d.get("models").and_then(|v| v.as_array()) {
        a.clone()
    } else {
        Vec::new()
    };
    arr.iter()
        .filter_map(|x| {
            if let Some(s) = x.as_str() {
                Some(s.to_string())
            } else {
                x.get("name").or_else(|| x.get("model")).and_then(|v| v.as_str()).map(String::from)
            }
        })
        .collect()
}

#[component]
fn ModelRow(model: Value, progress: Signal<HashMap<String, String>>, installed_refresh: Signal<u32>) -> Element {
    let name = model.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let fit_level = model.get("fit_level").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let fit_label = model.get("fit_label").and_then(|v| v.as_str()).unwrap_or(&fit_level).to_string();
    let meta = model_meta(&model);
    let prog_text = progress.read().get(&name).cloned().unwrap_or_default();
    let busy = {
        let p = progress.read();
        p.get(&name).map(|s| !s.is_empty() && !s.starts_with("complete") && !s.starts_with("error")).unwrap_or(false)
    };

    let start_download = {
        let name = name.clone();
        move |_| {
            let name = name.clone();
            let mut progress = progress;
            let mut installed_refresh = installed_refresh;
            spawn(async move {
                progress.write().insert(name.clone(), "starting…".to_string());
                let started = api::post_json("/api/llmfit/download", json!({ "model": name })).await;
                let id = match started {
                    Ok(v) => v.get("id").and_then(|x| x.as_str()).map(String::from),
                    Err(e) => {
                        progress.write().insert(name.clone(), format!("error: {e}"));
                        return;
                    }
                };
                let Some(id) = id else {
                    progress.write().insert(name.clone(), "error: no download id".to_string());
                    return;
                };
                loop {
                    gloo_timers::future::TimeoutFuture::new(1200).await;
                    match api::get_json(&format!("/api/llmfit/download/{id}/status")).await {
                        Ok(s) => {
                            let status = s.get("status").and_then(|v| v.as_str()).unwrap_or("");
                            let pct = s.get("progress_pct").and_then(|v| v.as_f64()).map(|p| format!("{}%", p.round() as i64)).unwrap_or_default();
                            let msg = s.get("message").and_then(|v| v.as_str()).unwrap_or("");
                            progress.write().insert(name.clone(), format!("{status} {pct} {msg}").split_whitespace().collect::<Vec<_>>().join(" "));
                            let done = matches!(status, "complete" | "completed" | "success" | "error" | "failed");
                            if done {
                                installed_refresh += 1;
                                break;
                            }
                        }
                        Err(e) => {
                            progress.write().insert(name.clone(), format!("error: {e}"));
                            break;
                        }
                    }
                }
            });
        }
    };

    rsx! {
        div { class: "card-pad flex items-center justify-between gap-3",
            div { class: "min-w-0",
                div { class: "font-mono text-fg-strong truncate", "{name}" }
                div { class: "help-xs td-muted", "{meta}" }
            }
            div { class: "flex items-center gap-2 shrink-0",
                Pill { variant: fit_variant(&fit_level), "{fit_label}" }
                Button {
                    size: ButtonSize::Xs,
                    variant: ButtonVariant::Accent,
                    disabled: busy,
                    onclick: start_download,
                    {t!("btn-download")}
                }
                span { class: "help-xs td-muted", "{prog_text}" }
            }
        }
    }
}
