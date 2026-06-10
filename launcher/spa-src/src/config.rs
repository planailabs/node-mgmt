//! Config tab — reuses the SHARED schema-driven editor
//! (`mac_mgmt_config_ui::ConfigEditor`, the same component the mac-mgmt server +
//! overview use) instead of a bespoke form. It loads the reduced UsbConfig +
//! its JSON Schema from the launcher's `/api/config*` (which proxies to the USB
//! daemon), and PUTs the edited config back on save. Mirrors
//! `mac-mgmt-overview::ConfigView`.

use dioxus::prelude::*;
use mac_mgmt_config_ui::ConfigEditor;
use plan_ai_design::Card;

use crate::api;

#[allow(non_snake_case)]
pub fn ConfigView() -> Element {
    let mut config = use_resource(|| async move { api::config().await.ok() });
    let schema = use_resource(|| async move { api::config_schema().await.ok() });
    let mut saving = use_signal(|| false);
    let mut save_err = use_signal(|| None::<String>);
    let mut saved_note = use_signal(|| None::<String>);

    let cfg = config.read().clone().flatten();
    let sch = schema.read().clone().flatten();

    rsx! {
        div { class: "flex-1 min-h-0 overflow-auto",
            Card {
                if let Some(note) = saved_note.read().clone() {
                    p { class: "td-muted text-sm mb-2", "{note}" }
                }
                if let Some(err) = save_err.read().clone() {
                    p { class: "text-danger text-sm mb-2", "{err}" }
                }
                match (sch, cfg) {
                    (Some(schema), Some(initial)) => rsx! {
                        ConfigEditor {
                            cluster_id: "usb".to_string(),
                            schema,
                            initial,
                            saving: saving(),
                            save_error: save_err(),
                            on_save: move |json: String| {
                                saving.set(true);
                                saved_note.set(None);
                                spawn(async move {
                                    let parsed = serde_json::from_str::<serde_json::Value>(&json);
                                    match parsed {
                                        Ok(v) => match api::set_config(v).await {
                                            Ok(_) => {
                                                save_err.set(None);
                                                saved_note.set(Some(
                                                    "Saved and applied — newly enabled services start in the background.".into(),
                                                ));
                                                config.restart();
                                            }
                                            Err(e) => save_err.set(Some(e)),
                                        },
                                        Err(e) => save_err.set(Some(format!("bad config JSON: {e}"))),
                                    }
                                    saving.set(false);
                                });
                            },
                        }
                    },
                    _ => rsx! { p { class: "card-pad td-muted text-sm", "Loading config…" } },
                }
            }
        }
    }
}
