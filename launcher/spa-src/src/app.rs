//! App shell: shared state, top chrome (tabs + theme), live status/log feeds.

use dioxus::prelude::*;
use dioxus_i18n::prelude::*;
use dioxus_i18n::t;
use futures_util::StreamExt;
use gloo_net::eventsource::futures::EventSource;
use unic_langid::langid;

use plan_ai_design::language_picker::LanguagePicker;
use plan_ai_design::theme_toggle::{
    ThemeToggle, THEME_INIT_SCRIPT, WASM_LOADING_INNER, WASM_LOADING_STYLE,
};
use plan_ai_design::{Button, ButtonSize, ButtonVariant};

use crate::api::{self, ConnectionStatus, Info, Platforms, ServiceState, ServiceStatus};
use crate::config::ConfigView;
use crate::dashboard::Dashboard;
use crate::models::Models;

#[derive(Clone, Copy, PartialEq)]
pub enum Tab {
    Dashboard,
    Models,
    WebUi,
    /// The llmfit model-browser dashboard (llmfit's own web UI, served on its
    /// port) — available whenever llmfit is running.
    Llmfit,
    /// The Hermes agent dashboard — shown when the "hermes" feature is enabled.
    Hermes,
    /// The Hermes web UI — same "hermes" feature, separate component + service.
    HermesWebUi,
    /// The Memvault web UI — shown when memvault is enabled and the daemon
    /// reports its (effective) port.
    Memvault,
    /// The Config tab — core (always available).
    Config,
}


/// State shared across views (provided via context; signals are Copy).
#[derive(Clone, Copy)]
pub struct AppState {
    pub info: Signal<Option<Info>>,
    pub services: Signal<Vec<ServiceStatus>>,
    pub logs: Signal<String>,
    pub tab: Signal<Tab>,
    /// Enabled optional features from /api/platforms (drive selection).
    pub features: Signal<Vec<String>>,
    /// Remote-management connection health (/api/connection); None until first poll.
    pub connection: Signal<Option<ConnectionStatus>>,
}

impl AppState {
    /// Whether a service id is in the ready state right now.
    pub fn ready(&self, id: &str) -> bool {
        self.services.read().iter().any(|s| s.id == id && s.state == ServiceState::Ready)
    }
    /// Whether llmfit (and thus the Models tab) is available.
    pub fn llmfit_ready(&self) -> bool {
        self.info.read().as_ref().map(|i| i.llmfit_url.is_some()).unwrap_or(false)
    }
    /// Whether the optional hermes agent is enabled on this drive.
    pub fn hermes_enabled(&self) -> bool {
        self.features.read().iter().any(|f| f == "hermes")
    }
    /// Whether the hermes web UI service is ready right now.
    pub fn hermes_webui_ready(&self) -> bool {
        self.ready("hermes-webui")
    }
    /// Whether memvault is enabled — the daemon reports a memvault URL only when
    /// it's serving the in-process memvault web app. Drives the Memvault app.
    pub fn memvault_ready(&self) -> bool {
        self.info.read().as_ref().map(|i| i.memvault_url.is_some()).unwrap_or(false)
    }
}

#[allow(non_snake_case)]
pub fn App() -> Element {
    let mut i18n = use_init_i18n(|| {
        // Concatenate the shared plan-ai-design FTL (theme toggle, language
        // picker, data table), the shared config-editor FTL (the Config tab
        // reuses mac_mgmt_config_ui::ConfigEditor, whose `t!` keys live there),
        // and this app's own FTL so `t!` resolves all three.
        let en: &'static str = Box::leak(
            format!("{}\n{}\n{}", plan_ai_design::i18n::EN_US, mac_mgmt_config_ui::EN_US, include_str!("../i18n/en-US.ftl"))
                .into_boxed_str(),
        );
        let de: &'static str = Box::leak(
            format!("{}\n{}\n{}", plan_ai_design::i18n::DE_DE, mac_mgmt_config_ui::DE_DE, include_str!("../i18n/de-DE.ftl"))
                .into_boxed_str(),
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
        features: use_signal(Vec::new),
        connection: use_signal(|| None),
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

    // Poll the drive selection every few seconds — the optional tabs (e.g. Hermes)
    // appear/disappear live as features are toggled on the dashboard.
    use_future(move || {
        let mut features = state.features;
        async move {
            loop {
                if let Ok(p) = api::get::<Platforms>("/api/platforms").await {
                    features.set(p.features);
                }
                gloo_timers::future::TimeoutFuture::new(5000).await;
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

    // Poll the remote-management connection health every 5s.
    use_future(move || {
        let mut connection = state.connection;
        async move {
            loop {
                if let Ok(c) = api::get::<ConnectionStatus>("/api/connection").await {
                    connection.set(Some(c));
                }
                gloo_timers::future::TimeoutFuture::new(5000).await;
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

    // Desktop sidebar collapse (icon rail) — toggled by the dedicated button in
    // the sidebar footer, restored from / persisted to localStorage.
    let mut sidebar_collapsed = use_signal(|| false);
    use_effect(move || {
        spawn(async move {
            if let Ok(v) = document::eval(
                "try { return localStorage.getItem('nav.sidebar.collapsed') || '0'; } catch(e) { return '0'; }",
            ).await {
                if v.as_str() == Some("1") {
                    sidebar_collapsed.set(true);
                }
            }
        });
    });
    use_effect(move || {
        let v = if *sidebar_collapsed.read() { "1" } else { "0" };
        document::eval(&format!(
            "try {{ localStorage.setItem('nav.sidebar.collapsed', '{v}'); }} catch(e) {{}}"
        ));
    });
    // Mobile drawer open/closed — shared by the topbar hamburger and the drawer.
    let drawer_open = use_signal(|| false);

    let tab = (state.tab)();
    // Topbar status word: offline (no remote server configured) → disconnected
    // (configured but the relay isn't up) → connected (relay live).
    let (conn_status, conn_status_cls) = {
        let conn = state.connection.read();
        match conn.as_ref() {
            // connected → brand orange, disconnected → strong fg (white in dark),
            // offline → muted grey.
            Some(c) if c.networked && c.remote_configured && c.relay_connected => (t!("chrome-status-connected"), "text-brand"),
            Some(c) if c.networked && c.remote_configured => (t!("chrome-status-disconnected"), "text-fg-strong"),
            _ => (t!("chrome-status-offline"), "text-fg-muted"),
        }
    };
    // The app registry is the single source of truth: which apps exist, their
    // switcher presentation, readiness, and how each renders (native view vs
    // embedded iframe). The sidebar nav and the view area both derive from it.
    let apps = app_registry(&state);

    // Sidebar shell classes. We inline these (rather than the shared `.nav-side`,
    // which hides below `xl`/1280px) because the launcher window is 1200px wide —
    // so the persistent sidebar shows from `lg`/1024px up, with the mobile drawer
    // taking over only on the narrowest windows. Collapse slides it to width 0.
    const SIDE_BASE: &str = "hidden lg:flex lg:flex-col bg-surface-2 border-r border-line shrink-0 overflow-hidden transition-[width] duration-300 ease-in-out";
    let collapsed = *sidebar_collapsed.read();
    // Collapse is an icon rail (avatars only), not a slide-off — 64px wide.
    let side_cls = if collapsed { format!("{SIDE_BASE} w-[64px]") } else { format!("{SIDE_BASE} w-[220px]") };
    let toggle_cls = if collapsed {
        "shrink-0 border-t border-line flex items-center justify-center px-3 py-3 text-fg-muted hover:text-fg-strong hover:bg-surface-3 transition-colors"
    } else {
        "shrink-0 border-t border-line flex items-center gap-2 px-3 py-3 text-fg-muted hover:text-fg-strong hover:bg-surface-3 transition-colors"
    };

    rsx! {
        script { dangerous_inner_html: THEME_INIT_SCRIPT }
        document::Stylesheet { href: asset!("/assets/tailwind.css") }

        div { id: "wasm-loading", style: WASM_LOADING_STYLE, dangerous_inner_html: WASM_LOADING_INNER }

        // Shell mirrors the mac-mgmt-server design: a slim topbar (brand logo over
        // the sidebar column, controls right) over a row of [collapsible 220px
        // sidebar | main content]. Below xl the sidebar is hidden and the topbar
        // hamburger opens a mobile drawer with the same nav.
        div { class: "bg-canvas text-fg h-screen flex flex-col overflow-hidden",
            header { class: "topbar",
                div { class: "topbar-logo-pad gap-2.5",
                    Logo {}
                }
                div { class: "topbar-controls",
                    // Styled like the mgmt wordmark suffix ("plan.ai mgmt"): muted,
                    // medium-weight, normal case — reads as a continuation of the
                    // brand rather than a separate uppercase label.
                    span { class: "mr-auto hidden sm:block text-sm font-medium text-fg-muted whitespace-nowrap",
                        {t!("chrome-tagline")} " · "
                        span { class: conn_status_cls, {conn_status} }
                    }
                    LanguagePicker {}
                    ThemeToggle {}
                    Button {
                        size: ButtonSize::Sm,
                        variant: ButtonVariant::Ghost,
                        class: "hidden sm:inline-flex",
                        // Electron routes window.open(external) to the system browser
                        // (setWindowOpenHandler → shell.openExternal in main/index.js).
                        onclick: move |_| {
                            document::eval("window.open('https://git.plan.ai/plan-ai/usb', '_blank');");
                        },
                        {t!("btn-report-issue")}
                    }
                    MobileMenuButton { open: drawer_open }
                }
            }

            div { class: "flex flex-1 overflow-hidden",
                // Desktop sidebar — shown from `lg` up (see SIDE_BASE) + width
                // transition; `.nav-side-collapsed` slides it to width 0.
                aside { class: "{side_cls}",
                    div { class: "flex flex-col h-full min-h-0",
                        NavContent { collapsed }
                        // Dedicated collapse toggle — kept separate from the logo
                        // (the logo is brand only, not a control).
                        button {
                            class: toggle_cls,
                            "aria-label": t!("nav-toggle-sidebar"),
                            onclick: move |_| {
                                let mut c = sidebar_collapsed;
                                let v = !*c.read();
                                c.set(v);
                            },
                            svg {
                                class: "h-4 w-4 shrink-0", fill: "none", stroke: "currentColor",
                                stroke_width: "2", view_box: "0 0 24 24",
                                path {
                                    stroke_linecap: "round", stroke_linejoin: "round",
                                    d: if collapsed { "M9 5l7 7-7 7" } else { "M15 19l-7-7 7-7" },
                                }
                            }
                            if !collapsed {
                                span { class: "text-xs font-medium", {t!("nav-collapse")} }
                            }
                        }
                    }
                }

                // The view area is driven by the current tab (the state machine's
                // state). Native views mount only while their tab is active. Embedded
                // (iframe) apps mount once ready and stay mounted — hidden when
                // inactive — so their session + scroll survive tab switches; one
                // uniform IframeView lifecycle, no per-app special-casing.
                div { class: "flex-1 flex flex-col min-w-0 overflow-hidden",
                    {match tab {
                        Tab::Dashboard => rsx! { Dashboard {} },
                        Tab::Models => rsx! { Models {} },
                        Tab::Config => rsx! { ConfigView {} },
                        _ => rsx! {},
                    }}
                    for app in apps.iter().filter(|a| matches!(a.kind, AppKind::Iframe { .. })) {
                        {
                            let src = match &app.kind {
                                AppKind::Iframe { src } => src.clone(),
                                AppKind::Native => None,
                            };
                            rsx! { IframeView { key: "{app.letter}", active: tab == app.tab, label: app.label.clone(), src } }
                        }
                    }
                }
            }

            MobileDrawer { open: drawer_open }
        }
    }
}

/// How an app's view is rendered.
#[derive(Clone, PartialEq)]
enum AppKind {
    /// A native Dioxus view, matched on the active tab (mounted only while active).
    Native,
    /// An external app embedded in an iframe; `src` is `Some` only once ready.
    Iframe { src: Option<String> },
}

/// A launchable app: its tab (the state-machine state), avatar presentation,
/// readiness, and how its view renders. The single source of truth shared by
/// the sidebar nav ([`NavContent`]) and the view area.
struct AppDesc {
    tab: Tab,
    label: String,
    letter: &'static str,
    avatar: &'static str,
    ready: bool,
    kind: AppKind,
}

/// Build the app registry from current state. Hermes' two apps appear only when
/// the feature is enabled. Iframe `src` is gated on readiness (`None` until the
/// service is up) so the iframe mounts only once it can actually load.
fn app_registry(state: &AppState) -> Vec<AppDesc> {
    let info = state.info.read();
    let url = |f: fn(&Info) -> Option<String>, ready: bool| info.as_ref().and_then(f).filter(|_| ready);

    let webui_ready = state.ready("webui");
    let llmfit_ready = state.llmfit_ready();
    let mut apps = vec![
        AppDesc {
            tab: Tab::WebUi, avatar: "bg-warn", label: t!("tab-webui"), letter: "O",
            ready: webui_ready,
            kind: AppKind::Iframe { src: url(|i| Some(i.webui_url.clone()), webui_ready) },
        },
        AppDesc {
            tab: Tab::Models, avatar: "bg-info", label: t!("tab-models"), letter: "M",
            ready: llmfit_ready, kind: AppKind::Native,
        },
        AppDesc {
            tab: Tab::Llmfit, avatar: "bg-info", label: t!("tab-llmfit"), letter: "L",
            ready: llmfit_ready,
            kind: AppKind::Iframe { src: url(|i| i.llmfit_url.clone(), llmfit_ready) },
        },
    ];
    if state.hermes_enabled() {
        let hermes_ready = state.ready("hermes-dashboard");
        let hermes_webui_ready = state.hermes_webui_ready();
        apps.push(AppDesc {
            tab: Tab::Hermes, avatar: "bg-warn", label: t!("tab-hermes"), letter: "H",
            ready: hermes_ready,
            kind: AppKind::Iframe { src: url(|i| i.hermes_url.clone(), hermes_ready) },
        });
        apps.push(AppDesc {
            tab: Tab::HermesWebUi, avatar: "bg-warn", label: t!("tab-hermes-webui"), letter: "W",
            ready: hermes_webui_ready,
            kind: AppKind::Iframe { src: url(|i| i.hermes_webui_url.clone(), hermes_webui_ready) },
        });
    }
    // Memvault is config-driven (not a platform feature): the daemon reports a
    // memvault_url only when it's enabled + serving, so that presence both adds the
    // app and gates the iframe src — same pattern as llmfit.
    if state.memvault_ready() {
        apps.push(AppDesc {
            tab: Tab::Memvault, avatar: "bg-info", label: t!("tab-memvault"), letter: "V",
            ready: true,
            kind: AppKind::Iframe { src: url(|i| i.memvault_url.clone(), true) },
        });
    }
    apps
}

/// An embedded app's iframe with the shared mount-once-ready lifecycle: the
/// frame enters the DOM only when `src` is `Some` (the service is ready), stays
/// mounted but `hidden` when its tab isn't active (preserving session + scroll),
/// and is torn out / re-mounted fresh if readiness drops. The not-ready notice
/// names the app (`label`) so it reads correctly for every app — not just
/// Open-WebUI.
#[component]
fn IframeView(active: bool, label: String, src: Option<String>) -> Element {
    let cls = if active { "flex-1 min-h-0" } else { "hidden" };
    rsx! {
        div { class: "{cls}",
            if let Some(url) = src {
                iframe { class: "w-full h-full border-0", src: "{url}" }
            } else {
                div { class: "card-pad td-muted text-sm", {t!("app-not-ready", name: label.clone())} }
            }
        }
    }
}

/// The grouped nav list shared by the desktop sidebar and the mobile drawer.
/// `on_navigate` fires after a tab switch so the drawer can close itself.
#[component]
fn NavContent(
    #[props(default)] on_navigate: Option<EventHandler<()>>,
    #[props(default)] collapsed: bool,
) -> Element {
    let state = use_context::<AppState>();
    let tab = (state.tab)();
    let apps = app_registry(&state);
    let nav_cls = if collapsed {
        "flex-1 min-h-0 overflow-y-auto px-2 py-4 space-y-3"
    } else {
        "flex-1 min-h-0 overflow-y-auto px-3 py-5 space-y-5"
    };
    rsx! {
        nav { class: nav_cls,
            div {
                if !collapsed {
                    h3 { class: "nav-group-head px-3 mb-2", {t!("nav-launcher")} }
                }
                div { class: "space-y-0.5",
                    NavItem { tab: Tab::Dashboard, current: tab, label: t!("tab-dashboard"), letter: "D", avatar: "bg-brand", enabled: true, collapsed, on_navigate }
                    // Config is core now (was the "mgmt" feature) — always available.
                    NavItem { tab: Tab::Config, current: tab, label: t!("tab-config"), letter: "C", avatar: "bg-brand", enabled: true, collapsed, on_navigate }
                }
            }
            div {
                // When collapsed the group label has no room — a hairline keeps the
                // launcher/apps split legible without text.
                if collapsed {
                    div { class: "h-px bg-line mx-1 my-1" }
                } else {
                    h3 { class: "nav-group-head px-3 mb-2", {t!("app-switcher-label")} }
                }
                div { class: "space-y-0.5",
                    for app in apps.iter() {
                        NavItem {
                            key: "{app.letter}",
                            tab: app.tab,
                            current: tab,
                            label: app.label.clone(),
                            letter: app.letter,
                            avatar: app.avatar,
                            enabled: app.ready,
                            collapsed,
                            on_navigate,
                        }
                    }
                }
            }
        }
    }
}

/// A single sidebar nav entry — a tab button on the shared `.nav-link` /
/// `.nav-link-active` rail, fronted by the waffle's coloured avatar circle
/// (`letter` over `avatar` bg) for app identity. While a service is still
/// starting the row greys out, stays unclickable, and shows a quiet hint.
#[component]
fn NavItem(
    tab: Tab,
    current: Tab,
    label: String,
    letter: &'static str,
    avatar: &'static str,
    enabled: bool,
    #[props(default)] collapsed: bool,
    #[props(default)] on_navigate: Option<EventHandler<()>>,
) -> Element {
    let state = use_context::<AppState>();
    let active = tab == current;
    let cls = if active { "nav-link nav-link-active" } else { "nav-link" };
    let avatar_cls = if enabled { avatar } else { "bg-surface-3" };
    // Collapsed: avatar only, centred. The label still rides along as a native
    // tooltip so the icon rail stays discoverable.
    let btn_cls = if collapsed {
        format!("{cls} w-full justify-center px-2 disabled:opacity-40 disabled:cursor-default disabled:hover:bg-transparent")
    } else {
        format!("{cls} w-full gap-2.5 disabled:opacity-40 disabled:cursor-default disabled:hover:bg-transparent")
    };
    rsx! {
        button {
            class: btn_cls,
            title: if collapsed { label.clone() } else { String::new() },
            disabled: !enabled,
            onclick: move |_| {
                if enabled {
                    let mut t = state.tab;
                    t.set(tab);
                    if let Some(h) = on_navigate { h.call(()); }
                }
            },
            span {
                class: "shrink-0 h-6 w-6 rounded-full grid place-items-center text-fg-invert text-[11px] font-semibold {avatar_cls}",
                "{letter}"
            }
            if !collapsed {
                span { class: "truncate", "{label}" }
                if !enabled {
                    span { class: "ml-auto help-xs td-muted", {t!("state-starting")} }
                }
            }
        }
    }
}

/// Orange brand mark (rounded-square outline + filled brand square) + wordmark.
#[allow(non_snake_case)]
fn LogoMark() -> Element {
    rsx! {
        svg {
            class: "shrink-0", width: "20", height: "20",
            view_box: "0 0 20 20", fill: "none",
            rect {
                x: "1.5", y: "1.5", width: "17", height: "17", rx: "5",
                stroke: "rgb(var(--c-brand))", stroke_width: "1.6",
            }
            rect {
                x: "6", y: "6", width: "8", height: "8", rx: "1.5",
                fill: "rgb(var(--c-brand))",
            }
        }
        // Wordmark matches the mgmt design: sans-serif, semibold, single
        // foreground colour (no mono, no orange split) — the brand colour lives
        // in the logo mark, not the text.
        span { class: "text-fg-strong font-semibold text-sm tracking-tight whitespace-nowrap",
            "plan.ai"
        }
    }
}

/// Static brand mark in the topbar's logo pad — purely identity, not a control
/// (the sidebar collapse toggle lives in the sidebar itself).
#[allow(non_snake_case)]
fn Logo() -> Element {
    rsx! {
        div { class: "flex items-center gap-2.5 select-none",
            LogoMark {}
        }
    }
}

/// Hamburger that opens the mobile drawer; shown only below `xl`.
#[component]
fn MobileMenuButton(open: Signal<bool>) -> Element {
    let is = *open.read();
    rsx! {
        button {
            class: "lg:hidden nav-icon-btn",
            "aria-label": t!("nav-open-main-menu"),
            "aria-expanded": "{is}",
            onclick: move |_| { let mut o = open; o.set(!is); },
            svg {
                class: "h-5 w-5", fill: "none", stroke: "currentColor", view_box: "0 0 24 24",
                if is {
                    path { stroke_linecap: "round", stroke_linejoin: "round", stroke_width: "2", d: "M6 18L18 6M6 6l12 12" }
                } else {
                    path { stroke_linecap: "round", stroke_linejoin: "round", stroke_width: "2", d: "M4 6h16M4 12h16M4 18h16" }
                }
            }
        }
    }
}

/// Slide-in drawer for narrow viewports — same `NavContent` as the desktop
/// sidebar, behind a tap-to-close backdrop. Hidden on `xl`.
#[component]
fn MobileDrawer(open: Signal<bool>) -> Element {
    let is = *open.read();
    let backdrop = if is {
        "fixed inset-0 bg-bg/80 backdrop-blur-sm transition-opacity duration-300 z-40 opacity-100 pointer-events-auto"
    } else {
        "fixed inset-0 bg-bg/80 backdrop-blur-sm transition-opacity duration-300 z-40 opacity-0 pointer-events-none"
    };
    let panel = if is {
        "fixed inset-y-0 right-0 max-w-xs w-full bg-surface shadow-xl flex flex-col z-50 transform transition-transform duration-300 ease-in-out border-l border-line translate-x-0 pointer-events-auto"
    } else {
        "fixed inset-y-0 right-0 max-w-xs w-full bg-surface shadow-xl flex flex-col z-50 transform transition-transform duration-300 ease-in-out border-l border-line translate-x-full pointer-events-none"
    };
    rsx! {
        div { class: "lg:hidden relative z-50",
            div {
                class: backdrop,
                "aria-hidden": "true",
                onclick: move |_| { let mut o = open; o.set(false); },
            }
            div { class: panel, id: "mobile-drawer",
                div { class: "px-5 py-4 bg-surface-2 border-b border-line flex items-center justify-between shrink-0",
                    span { class: "kicker", {t!("app-switcher-label")} }
                    button {
                        class: "nav-icon-btn",
                        "aria-label": t!("nav-close-menu"),
                        onclick: move |_| { let mut o = open; o.set(false); },
                        svg {
                            class: "h-5 w-5", fill: "none", stroke: "currentColor", view_box: "0 0 24 24",
                            path { stroke_linecap: "round", stroke_linejoin: "round", stroke_width: "2", d: "M6 18L18 6M6 6l12 12" }
                        }
                    }
                }
                NavContent { on_navigate: move |_| { let mut o = open; o.set(false); } }
            }
        }
    }
}
