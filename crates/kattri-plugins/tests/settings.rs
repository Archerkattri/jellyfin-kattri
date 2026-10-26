//! M4-07: plugin settings namespace. Validation against declared keys, grant gating,
//! namespace isolation and the credential refusals.

use kattri_plugins::settings::{MAX_SETTING_VALUE_BYTES, PluginSettingsStore};
use kattri_plugins::{Permission, PermissionGrants, PluginError, PluginManifest};
use serde_json::json;

fn manifest(id: &str, keys: &[&str]) -> PluginManifest {
    let text = json!({
        "id": id,
        "name": "Settings Test",
        "version": "1.0.0",
        "api_version": 1,
        "entry": "main.wasm",
        "permissions": ["settings.own"],
        "settings": keys,
    })
    .to_string();
    PluginManifest::from_json(&text).expect("test manifest is valid")
}

fn granted(m: &PluginManifest) -> PermissionGrants {
    let mut grants = PermissionGrants::for_plugin(m);
    grants.grant(Permission::SettingsOwn);
    grants
}

#[test]
fn declared_key_round_trips_within_the_plugin_namespace() {
    let m = manifest("org.example.a", &["skip_intro"]);
    let grants = granted(&m);
    let mut store = PluginSettingsStore::new();
    assert_eq!(store.get(&m, &grants, "skip_intro"), Ok(None));
    store
        .set(&m, &grants, "skip_intro", "true")
        .expect("declared key");
    assert_eq!(store.get(&m, &grants, "skip_intro"), Ok(Some("true")));
    assert_eq!(
        store.remove(&m, &grants, "skip_intro"),
        Ok(Some("true".to_owned()))
    );
    assert_eq!(store.get(&m, &grants, "skip_intro"), Ok(None));
}

#[test]
fn undeclared_key_is_refused_and_stores_nothing() {
    let m = manifest("org.example.a", &["skip_intro"]);
    let grants = granted(&m);
    let mut store = PluginSettingsStore::new();
    let err = store
        .set(&m, &grants, "other", "1")
        .expect_err("undeclared");
    assert!(matches!(
        err,
        PluginError::Invalid {
            field: "settings",
            ..
        }
    ));
    assert!(store.table("org.example.a").is_none());
}

#[test]
fn settings_write_without_grant_is_permission_denied() {
    let m = manifest("org.example.a", &["skip_intro"]);
    let no_grant = PermissionGrants::for_plugin(&m);
    let mut store = PluginSettingsStore::new();
    assert_eq!(
        store.set(&m, &no_grant, "skip_intro", "true"),
        Err(PluginError::PermissionDenied {
            permission: "settings.own".to_owned()
        })
    );
    assert!(store.table("org.example.a").is_none());
}

#[test]
fn unbound_grants_authorize_nothing() {
    let m = manifest("org.example.a", &["skip_intro"]);
    let mut unbound = PermissionGrants::new();
    unbound.grant(Permission::SettingsOwn);
    let mut store = PluginSettingsStore::new();
    assert!(matches!(
        store.set(&m, &unbound, "skip_intro", "true"),
        Err(PluginError::PermissionDenied { .. })
    ));
}

#[test]
fn namespaces_are_isolated_between_plugins() {
    let a = manifest("org.example.a", &["shared_name"]);
    let b = manifest("org.example.b", &["shared_name"]);
    let ga = granted(&a);
    let gb = granted(&b);
    let mut store = PluginSettingsStore::new();
    store
        .set(&a, &ga, "shared_name", "from-a")
        .expect("a writes");
    assert_eq!(
        store.get(&b, &gb, "shared_name"),
        Ok(None),
        "plugin B must not read plugin A's value"
    );
    store
        .set(&b, &gb, "shared_name", "from-b")
        .expect("b writes");
    assert_eq!(store.get(&a, &ga, "shared_name"), Ok(Some("from-a")));
}

#[test]
fn grant_on_one_plugin_does_not_open_another_plugins_namespace() {
    let a = manifest("org.example.a", &["k"]);
    let b = manifest("org.example.b", &["k"]);
    let gb = PermissionGrants::for_plugin(&b);
    let ga = granted(&a);
    let mut store = PluginSettingsStore::new();
    store.set(&a, &ga, "k", "v").expect("a writes");
    assert!(store.get(&b, &gb, "k").is_err(), "b has no grant");
    assert!(
        store.get(&b, &ga, "k").is_err(),
        "a's grants are bound to a and must not authorize b"
    );
}

#[test]
fn url_and_credential_looking_values_are_refused() {
    let m = manifest("org.example.a", &["note"]);
    let grants = granted(&m);
    let mut store = PluginSettingsStore::new();
    for bad in [
        "https://media.example.org/stream",
        "http://192.168.1.10:8096",
        "api_key=abc123",
        "token=abc123",
        "see HTTPS://X",
    ] {
        assert!(
            matches!(
                store.set(&m, &grants, "note", bad),
                Err(PluginError::Invalid {
                    field: "settings.value",
                    ..
                })
            ),
            "{bad:?} must be refused"
        );
    }
    assert!(
        store.table("org.example.a").is_none(),
        "nothing stored after refusals"
    );
}

#[test]
fn value_size_and_control_characters_are_bounded() {
    let m = manifest("org.example.a", &["note"]);
    let grants = granted(&m);
    let mut store = PluginSettingsStore::new();
    let at_limit = "x".repeat(MAX_SETTING_VALUE_BYTES);
    assert!(store.set(&m, &grants, "note", &at_limit).is_ok());
    let over = "x".repeat(MAX_SETTING_VALUE_BYTES + 1);
    assert!(matches!(
        store.set(&m, &grants, "note", &over),
        Err(PluginError::Invalid {
            field: "settings.value",
            ..
        })
    ));
    assert!(matches!(
        store.set(&m, &grants, "note", "line\nbreak"),
        Err(PluginError::Invalid {
            field: "settings.value",
            ..
        })
    ));
}

#[test]
fn manifest_refuses_credential_like_or_duplicate_declared_keys() {
    for bad_keys in [
        json!(["api_token"]),
        json!(["password"]),
        json!(["a", "a"]),
        json!([""]),
    ] {
        let text = json!({
            "id": "org.example.a",
            "name": "X",
            "version": "1.0.0",
            "api_version": 1,
            "entry": "main.wasm",
            "permissions": ["settings.own"],
            "settings": bad_keys,
        })
        .to_string();
        assert!(
            PluginManifest::from_json(&text).is_err(),
            "declared keys {bad_keys} must be refused"
        );
    }
}

#[test]
fn manifest_without_settings_field_declares_no_keys() {
    let text = json!({
        "id": "org.example.a",
        "name": "X",
        "version": "1.0.0",
        "api_version": 1,
        "entry": "main.wasm",
    })
    .to_string();
    let m = PluginManifest::from_json(&text).expect("valid");
    assert!(m.settings.is_empty());
}

#[test]
fn credential_text_is_never_written_by_a_host_settings_path() {
    // Grep-style check from the spec: a sample token placed in any setting is not stored.
    let sample_token = "tok_SAMPLE_0123456789abcdef";
    let m = manifest("org.example.a", &["note"]);
    let grants = granted(&m);
    let mut store = PluginSettingsStore::new();
    let _ = store.set(&m, &grants, "note", &format!("token={sample_token}"));
    let stored = store
        .table("org.example.a")
        .map(|t| t.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        stored.iter().all(|v| !v.contains(sample_token)),
        "sample token must not be stored"
    );
}
