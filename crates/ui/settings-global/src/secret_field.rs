//! `SecretField.tsx`: one credential input. Each field handles masking, reveal, saving on blur and deleting on empty.

use std::collections::HashMap;

use mewrk_client::prelude::*;
use serde::Deserialize;

use crate::logic::store;
use crate::logic::web_search::{search_key_length_store_key, SearchCredentialSlot};
use crate::widgets::*;

/// The status the credential commands answer with.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyStatus {
    pub configured: bool,
    #[serde(default)]
    pub key_length: Option<usize>,
}

/// A saved answer is an object here, but upstream's command also tolerates a bool or null.
fn status_of(value: &Value) -> ApiKeyStatus {
    match value {
        Value::Object(_) => serde_json::from_value(value.clone()).unwrap_or(ApiKeyStatus { configured: true, key_length: None }),
        other => ApiKeyStatus { configured: other.as_bool() != Some(false), key_length: None },
    }
}

/// One slot of a search provider's secrets. The host validates both against its own catalog.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Identity {
    pub provider_kind: String,
    pub slot: SearchCredentialSlot,
}

#[derive(Default)]
struct FieldData {
    started: bool,
    draft: Option<String>,
    key_length: usize,
    saving: bool,
    revealing: bool,
    visible: bool,
    status_token: u64,
    save_token: u64,
    saved_draft: Option<String>,
}

#[derive(Default)]
pub struct SecretFields {
    fields: HashMap<Identity, FieldData>,
    /// Bumps when the page unmounts: answers for an earlier mount have nowhere to go.
    generation: u64,
}

fn data<'a>(app: &'a mut App, id: &Identity) -> &'a mut FieldData {
    app.local::<SecretFields>().fields.entry(id.clone()).or_default()
}

/// What the field looks like right now, for tests.
pub fn field_state(app: &mut App, id: &Identity) -> (Option<String>, usize, bool) {
    let d = data(app, id);
    (d.draft.clone(), d.key_length, d.visible)
}

pub fn unmount(app: &mut App) {
    let fields = app.local::<SecretFields>();
    fields.fields.clear();
    fields.generation += 1;
}

/// The field's identity changed: what was typed or revealed for the last provider doesn't follow it, and coming back reads the status again.
pub fn identity_changed(app: &mut App) {
    for d in app.local::<SecretFields>().fields.values_mut() {
        d.started = false;
        d.status_token += 1;
        d.draft = None;
        d.saved_draft = None;
        d.visible = false;
    }
}

fn remember_length(app: &mut App, id: &Identity, length: Option<usize>) {
    let key = search_key_length_store_key(&id.provider_kind, id.slot);
    app.ui_store_update(|store| match length {
        Some(n) if n > 0 => store.set(&key, n.to_string()),
        _ => store.remove(&key),
    });
}

fn stored_length(app: &mut App, id: &Identity) -> Option<usize> {
    let key = search_key_length_store_key(&id.provider_kind, id.slot);
    app.ui_store().get(&key).and_then(|s| s.parse().ok()).filter(|n| *n > 0)
}

fn args(id: &Identity) -> Value {
    json!({ "providerKind": id.provider_kind, "slot": id.slot.wire() })
}

fn refresh_status(app: &mut App, id: &Identity) {
    let d = data(app, id);
    d.status_token += 1;
    let token = d.status_token;
    let generation = app.local::<SecretFields>().generation;
    let id2 = id.clone();
    app.invoke::<Value>("get_search_key_status", args(id), move |app, result| {
        if app.local::<SecretFields>().generation != generation || data(app, &id2).status_token != token {
            return;
        }
        // A status-read failure is handled by explicit credential operations.
        let Ok(value) = result else { return };
        let status = status_of(&value);
        let length = if status.configured { status.key_length.or_else(|| stored_length(app, &id2)) } else { None };
        remember_length(app, &id2, length);
        data(app, &id2).key_length = length.unwrap_or(0);
    });
}

fn persist(app: &mut App, id: &Identity, done: impl FnOnce(&mut App, bool) + Send + 'static) {
    let d = data(app, id);
    let secret = d.draft.as_deref().unwrap_or("").trim().to_owned();
    if secret.is_empty() || d.saved_draft.as_deref() == Some(secret.as_str()) {
        done(app, true);
        return;
    }
    d.save_token += 1;
    let token = d.save_token;
    d.saving = true;
    let generation = app.local::<SecretFields>().generation;
    let id2 = id.clone();
    store::flush(app, move |app, flushed| {
        let stale = |app: &mut App| app.local::<SecretFields>().generation != generation || data(app, &id2).save_token != token;
        if flushed.is_err() {
            if !stale(app) {
                data(app, &id2).saving = false;
            }
            return done(app, false);
        }
        let mut body = args(&id2);
        body["apiKey"] = json!(secret);
        let id3 = id2.clone();
        app.invoke::<Value>("save_search_api_key", body, move |app, result| {
            if app.local::<SecretFields>().generation != generation || data(app, &id3).save_token != token {
                return done(app, false);
            }
            data(app, &id3).saving = false;
            match result {
                Ok(value) if value.as_bool() != Some(false) => {
                    let status = status_of(&value);
                    let length = status.key_length.unwrap_or_else(|| secret.chars().count());
                    remember_length(app, &id3, Some(length));
                    let d = data(app, &id3);
                    d.key_length = length;
                    if d.visible {
                        d.saved_draft = Some(secret);
                    } else {
                        d.saved_draft = None;
                        d.draft = None;
                    }
                    done(app, true);
                }
                _ => done(app, false),
            }
        });
    });
}

fn remove_stored(app: &mut App, id: &Identity) {
    let d = data(app, id);
    d.save_token += 1;
    let token = d.save_token;
    d.saving = true;
    let generation = app.local::<SecretFields>().generation;
    let id2 = id.clone();
    store::flush(app, move |app, flushed| {
        if flushed.is_err() {
            if app.local::<SecretFields>().generation == generation && data(app, &id2).save_token == token {
                data(app, &id2).saving = false;
            }
            return;
        }
        let id3 = id2.clone();
        app.invoke::<Value>("delete_search_api_key", args(&id2), move |app, result| {
            if app.local::<SecretFields>().generation != generation || data(app, &id3).save_token != token {
                return;
            }
            let d = data(app, &id3);
            d.saving = false;
            // A failed deletion keeps the stored credential so blur can retry.
            if result.is_ok() {
                d.saved_draft = None;
                d.draft = None;
                d.key_length = 0;
                remember_length(app, &id3, None);
            }
        });
    });
}

fn toggle_visibility(app: &mut App, id: &Identity) {
    let d = data(app, id);
    if d.visible {
        let id2 = id.clone();
        persist(app, id, move |app, saved| {
            if saved {
                let d = data(app, &id2);
                d.visible = false;
                d.draft = None;
                d.saved_draft = None;
            }
        });
        return;
    }
    if d.draft.is_some() {
        d.visible = true;
        return;
    }
    d.revealing = true;
    let generation = app.local::<SecretFields>().generation;
    let id2 = id.clone();
    store::flush(app, move |app, flushed| {
        let live = |app: &mut App| app.local::<SecretFields>().generation == generation;
        if flushed.is_err() {
            if live(app) {
                data(app, &id2).revealing = false;
            }
            return;
        }
        let id3 = id2.clone();
        // Ask whether one is configured first: parsing the host's copy would tie this path to localized error text.
        app.invoke::<Value>("get_search_key_status", args(&id2), move |app, status| {
            if app.local::<SecretFields>().generation != generation {
                return;
            }
            let configured = status.map(|v| status_of(&v).configured);
            match configured {
                Ok(false) => {
                    let d = data(app, &id3);
                    d.draft = Some(String::new());
                    d.visible = true;
                    d.revealing = false;
                }
                Ok(true) => {
                    let id4 = id3.clone();
                    app.invoke::<String>("reveal_search_api_key", args(&id3), move |app, secret| {
                        if app.local::<SecretFields>().generation != generation {
                            return;
                        }
                        // Keep the field masked after a read failure so the user can replace it.
                        if let Ok(secret) = secret {
                            let length = secret.chars().count();
                            remember_length(app, &id4, Some(length));
                            let d = data(app, &id4);
                            d.saved_draft = Some(secret.trim().to_owned());
                            d.draft = Some(secret);
                            d.key_length = length;
                            d.visible = true;
                        }
                        data(app, &id4).revealing = false;
                    });
                }
                Err(_) => data(app, &id3).revealing = false,
            }
        });
    });
}

/// The field. `required` adds it to the label; `help` is the line under it.
pub fn secret_field(app: &mut App, id: Identity, label: &str, help_text: String, required: bool) -> E {
    if !data(app, &id).started {
        data(app, &id).started = true;
        refresh_status(app, &id);
    }
    let d = data(app, &id);
    let draft = d.draft.clone();
    let visible = d.visible;
    let (revealing, saving) = (d.revealing, d.saving);
    let mask = "\u{2022}".repeat(d.key_length);
    let has_mask = !mask.is_empty();
    let value = draft.clone().unwrap_or(mask);
    let title = if required { tf("{name}（必填）", "{name} (required)", &[("name", &label)]) } else { label.to_owned() };
    let reveal_label = if revealing {
        tf("正在读取{name}", "Reading {name}", &[("name", &label)])
    } else if visible {
        tf("隐藏{name}", "Hide {name}", &[("name", &label)])
    } else {
        tf("显示{name}", "Show {name}", &[("name", &label)])
    };
    let (change_id, blur_id, reveal_id) = (id.clone(), id.clone(), id.clone());
    let field_key = ("secret", id.provider_kind.clone(), id.slot.wire());
    let reveal_key = ("secret-reveal", id.provider_kind.clone(), id.slot.wire());
    let input_el = field::<App>(field_key, value)
        .placeholder(tf("输入{name}", "Enter {name}", &[("name", &label)]))
        .ty(MONO_BODY)
        .bare()
        .obscure(!visible)
        .controlled()
        .select_on_focus(draft.is_none() && has_mask)
        .on_change(move |app, v| data(app, &change_id).draft = Some(v))
        .on_blur(move |app, _| {
            let draft = data(app, &blur_id).draft.clone();
            if draft.is_some_and(|s| s.trim().is_empty()) {
                remove_stored(app, &blur_id);
            } else {
                persist(app, &blur_id, |_, _| {});
            }
        })
        .build()
        .flex1()
        .h(18.0)
        .aria_label(label)
        .aria_busy(saving);
    let reveal = reveal_button(reveal_key, reveal_label, revealing, visible, revealing || saving, move |app: &mut App| toggle_visibility(app, &reveal_id));
    col().gap(6.0).child(text(title).ty(CAPTION_STRONG).nowrap()).child(input_group(input_el, false).child(reveal)).child(help(help_text))
}
