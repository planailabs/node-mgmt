//! App shell: shared state, top chrome (tabs + theme), live status/log feeds.

use dioxus::prelude::*;
use dioxus_i18n::prelude::*;
use dioxus_i18n::t;
use futures_util::StreamExt;
use gloo_net::eventsource::futures::EventSource;
use serde_json::Value;
use unic_langid::langid;

use plan_ai_design::language_picker::LanguagePicker;
use plan_ai_design::theme_toggle::{
    ThemeToggle, THEME_INIT_SCRIPT, WASM_LOADING_INNER, WASM_LOADING_STYLE,
};
use plan_ai_design::{Button, ButtonSize, ButtonVariant};

use crate::api::{self, Service};
use crate::dashboard::Dashboard;
use crate::models::Models;

#[derive(Clone, Copy, PartialEq)]
pub enum Tab {
    Dashboard,
    Models,
    WebUi,
}

/// State shared across views (provided via context; signals are Copy).
#[derive(Clone, Copy)]
pub struct AppState {
    pub info: Signal<Option<Value>>,
    pub services: Signal<Vec<Service>>,
    pub logs: Signal<String>,
    pub tab: Signal<Tab>,
}

impl AppState {
    /// Whether a service id is in the "ready" state right now.
    pub fn ready(&self, id: &str) -> bool {
        self.services.read().iter().any(|s| s.id == id && s.state == "ready")
    }
    /// Whether llmfit (and thus the Models tab) is available.
    pub fn llmfit_ready(&self) -> bool {
        self.info
            .read()
            .as_ref()
            .and_then(|i| i.get("llmfit_url").cloned())
            .map(|v| !v.is_null())
            .unwrap_or(false)
    }
}

#[allow(non_snake_case)]
pub fn App() -> Element {
    let mut i18n = use_init_i18n(|| {
        // Concatenate the shared plan-ai-design FTL (theme toggle, language
        // picker, data table) with this app's own FTL so `t!` resolves both.
        let en: &'static str = Box::leak(
            format!("{}\n{}", plan_ai_design::i18n::EN_US, include_str!("../i18n/en-US.ftl")).into_boxed_str(),
        );
        let de: &'static str = Box::leak(
            format!("{}\n{}", plan_ai_design::i18n::DE_DE, include_str!("../i18n/de-DE.ftl")).into_boxed_str(),
        );
        I18nConfig::new(langid!("en-US"))
            .with_locale(Locale::new_static(langid!("en-US"), en))
            .with_locale(Locale::new_static(langid!("de-DE"), de))
    });

    // Restore the saved language (localStorage['lang'], set by the LanguagePicker).
    use_effect(move || {
        spawn(async move {
            if let Ok(v) = document::eval("try { return localStorage.getItem('lang') || ''; } catch(e) { return ''; }").await {
                if v.as_str() == Some("de-DE") {
                    let _ = i18n.set_language(langid!("de-DE"));
                }
            }
        });
    });

    let state = AppState {
        info: use_signal(|| None),
        services: use_signal(Vec::new),
        logs: use_signal(String::new),
        tab: use_signal(|| Tab::Dashboard),
    };
    use_context_provider(|| state);

    // Drop the pre-hydration loading banner once mounted.
    use_effect(|| {
        document::eval("document.getElementById('wasm-loading')?.remove();");
    });

    // One-shot: load /api/info.
    use_future(move || {
        let mut info = state.info;
        async move {
            if let Ok(v) = api::info().await {
                info.set(Some(v));
            }
        }
    });

    // Poll service status every 2s.
    use_future(move || {
        let mut services = state.services;
        async move {
            loop {
                if let Ok(list) = api::status().await {
                    services.set(list);
                }
                gloo_timers::future::TimeoutFuture::new(2000).await;
            }
        }
    });

    // Stream logs over SSE.
    use_future(move || {
        let mut logs = state.logs;
        async move {
            let Ok(mut es) = EventSource::new("/api/logs") else { return };
            let Ok(mut stream) = es.subscribe("message") else { return };
            while let Some(Ok((_, msg))) = stream.next().await {
                if let Some(line) = msg.data().as_string() {
                    let mut buf = logs.write();
                    buf.push_str(&line);
                    buf.push('\n');
                    // cap the buffer so long runs don't grow unbounded
                    if buf.len() > 200_000 {
                        let cut = buf.len() - 150_000;
                        *buf = buf[cut..].to_string();
                    }
                }
            }
            drop(es);
        }
    });

    let tab = (state.tab)();
    let webui_ready = state.ready("webui");
    let models_ready = state.llmfit_ready();
    let webui_url = state
        .info
        .read()
        .as_ref()
        .and_then(|i| i.get("webui_url").and_then(|v| v.as_str()).map(String::from));

    rsx! {
        script { dangerous_inner_html: THEME_INIT_SCRIPT }
        document::Stylesheet { href: asset!("/assets/tailwind.css") }

        div { id: "wasm-loading", style: WASM_LOADING_STYLE, dangerous_inner_html: WASM_LOADING_INNER }

        div { class: "bg-canvas text-fg h-screen flex flex-col overflow-hidden",
            // top chrome
            header { class: "topbar",
                div { class: "flex items-center gap-3",
                    span { class: "font-mono text-fg-strong font-semibold tracking-tight",
                        "plan"
                        span { class: "text-brand", ".ai" }
                    }
                    span { class: "kicker", {t!("chrome-tagline")} }
                }
                div { class: "topbar-controls flex items-center gap-2",
                    TabButton { tab: Tab::Dashboard, current: tab, label: t!("tab-dashboard"), enabled: true }
                    TabButton { tab: Tab::Models, current: tab, label: t!("tab-models"), enabled: models_ready }
                    TabButton { tab: Tab::WebUi, current: tab, label: t!("tab-webui"), enabled: webui_ready }
                    Button {
                        size: ButtonSize::Sm,
                        variant: ButtonVariant::Ghost,
                        // Electron routes window.open(external) to the system browser
                        // (setWindowOpenHandler → shell.openExternal in main/index.js).
                        onclick: move |_| {
                            document::eval("window.open('https://git.plan.ai/plan-ai/usb', '_blank');");
                        },
                        {t!("btn-report-issue")}
                    }
                    LanguagePicker {}
                    ThemeToggle {}
                }
            }

            // views
            match tab {
                Tab::Dashboard => rsx! { Dashboard {} },
                Tab::Models => rsx! { Models {} },
                Tab::WebUi => rsx! {
                    div { class: "flex-1 min-h-0",
                        if let Some(url) = webui_url {
                            iframe { class: "w-full h-full border-0", src: "{url}" }
                        } else {
                            div { class: "card-pad td-muted text-sm", {t!("webui-not-ready")} }
                        }
                    }
                },
            }
        }
    }
}

#[component]
fn TabButton(tab: Tab, current: Tab, label: String, enabled: bool) -> Element {
    let state = use_context::<AppState>();
    let active = tab == current;
    let variant = if active { ButtonVariant::Secondary } else { ButtonVariant::Ghost };
    rsx! {
        Button {
            size: ButtonSize::Sm,
            variant,
            disabled: !enabled,
            onclick: move |_| {
                if enabled {
                    let mut t = state.tab;
                    t.set(tab);
                }
            },
            "{label}"
        }
    }
}
