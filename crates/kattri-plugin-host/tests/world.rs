//! Host import tests for the `kattri-plugin` world. The import methods are called directly on
//! the host state, so these tests need no guest component.

use kattri_plugin_host::bindings::KattriPluginImports;
use kattri_plugin_host::world::HostState;
use kattri_plugins::settings::PluginSettingsStore;
use kattri_plugins::{Permission, PermissionGrants, PluginManifest};

const PRESENCE_JSON: &str = r#"{"state":"Watching","details":"Kitchen sink, season 2"}"#;

fn manifest(permissions: &str, settings: &str) -> PluginManifest {
    let text = format!(
        r#"{{"id":"org.kattri.sample-presence","name":"Sample","version":"0.1.0",
            "api_version":1,"entry":"src/lib.rs","permissions":[{permissions}],
            "settings":[{settings}]}}"#
    );
    PluginManifest::from_json(&text).expect("test manifest is valid")
}

fn host_with(manifest: PluginManifest, grants: &[Permission]) -> HostState {
    let mut permission_grants = PermissionGrants::for_plugin(&manifest);
    for permission in grants {
        permission_grants.grant(permission.clone());
    }
    HostState::new(manifest, permission_grants, PluginSettingsStore::new())
}

#[test]
fn presence_write_without_a_grant_is_refused() {
    let manifest = manifest(r#""presence.write""#, "");
    let mut host = host_with(manifest, &[]);

    let result = host.presence_write(PRESENCE_JSON.to_owned());

    let err = result.expect_err("no grant means no presence write");
    assert!(err.contains("denied") || err.contains("granted"), "{err}");
    assert!(
        host.last_presence().is_none(),
        "a refused write changes nothing"
    );
}

#[test]
fn presence_write_with_a_grant_is_recorded_after_validation() {
    let manifest = manifest(r#""presence.write""#, "");
    let mut host = host_with(manifest, &[Permission::PresenceWrite]);

    host.presence_write(PRESENCE_JSON.to_owned())
        .expect("granted and valid");

    let activity = host.last_presence().expect("activity recorded");
    assert_eq!(activity.state.as_deref(), Some("Watching"));
}

#[test]
fn presence_write_refuses_a_url_even_with_a_grant() {
    let manifest = manifest(r#""presence.write""#, "");
    let mut host = host_with(manifest, &[Permission::PresenceWrite]);

    let result = host.presence_write(r#"{"details":"http://10.0.0.5:8096/Items/1"}"#.to_owned());

    assert!(result.is_err());
    assert!(host.last_presence().is_none());
}

#[test]
fn presence_write_is_refused_when_the_manifest_does_not_declare_it() {
    let manifest = manifest(r#""log.write""#, "");
    let mut host = host_with(manifest, &[Permission::PresenceWrite]);

    assert!(host.presence_write(PRESENCE_JSON.to_owned()).is_err());
}

#[test]
fn log_write_needs_its_own_grant_and_tags_the_plugin_id() {
    let manifest = manifest(r#""log.write""#, "");
    let mut refused = host_with(manifest.clone(), &[]);
    assert!(refused.log_write("hello".to_owned()).is_err());
    assert!(refused.log_lines().is_empty());

    let mut granted = host_with(manifest, &[Permission::LogWrite]);
    granted.log_write("hello".to_owned()).expect("granted");
    assert_eq!(
        granted.log_lines(),
        ["[org.kattri.sample-presence] hello".to_owned()]
    );
}

#[test]
fn plugin_log_history_is_bounded_and_keeps_the_newest_lines() {
    let manifest = manifest(r#""log.write""#, "");
    let mut host = host_with(manifest, &[Permission::LogWrite]);
    for n in 0..=kattri_plugin_host::world::MAX_PLUGIN_LOG_LINES {
        host.log_write(format!("line-{n}"))
            .expect("authorized bounded log write");
    }
    assert_eq!(
        host.log_lines().len(),
        kattri_plugin_host::world::MAX_PLUGIN_LOG_LINES
    );
    assert_eq!(
        host.log_lines().first().map(String::as_str),
        Some("[org.kattri.sample-presence] line-1")
    );
    assert_eq!(
        host.log_lines().last().map(String::as_str),
        Some("[org.kattri.sample-presence] line-1024")
    );
}

#[test]
fn revoking_a_running_host_state_blocks_its_next_import() {
    let manifest = manifest(r#""presence.write""#, "");
    let mut host = host_with(manifest, &[Permission::PresenceWrite]);
    host.presence_write(PRESENCE_JSON.to_owned())
        .expect("initial grant allows write");
    host.revoke_permission(&Permission::PresenceWrite);
    let before = host.last_presence().cloned();
    assert!(host.presence_write(PRESENCE_JSON.to_owned()).is_err());
    assert_eq!(host.last_presence(), before.as_ref());
}

#[test]
fn settings_imports_need_the_settings_grant_and_a_declared_key() {
    let manifest = manifest(r#""settings.own""#, r#""quality""#);

    let mut refused = host_with(manifest.clone(), &[]);
    assert!(
        refused
            .settings_set("quality".to_owned(), "1080p".to_owned())
            .is_err()
    );
    assert!(refused.settings_get("quality".to_owned()).is_err());

    let mut host = host_with(manifest, &[Permission::SettingsOwn]);
    host.settings_set("quality".to_owned(), "1080p".to_owned())
        .expect("declared and granted");
    assert_eq!(
        host.settings_get("quality".to_owned()),
        Ok(Some("1080p".to_owned()))
    );
    assert!(
        host.settings_set("undeclared".to_owned(), "x".to_owned())
            .is_err(),
        "an undeclared key is refused"
    );
    assert!(
        host.settings_set("quality".to_owned(), "https://h.example.org".to_owned())
            .is_err(),
        "a URL-like value is refused"
    );
}

#[test]
#[ignore = "wasm32-wasip2 target is not installed, so the sample plugin component is not built"]
fn init_is_called_on_a_component_when_one_is_available() {
    let component_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/sample_presence.wasm"
    );
    let engine = kattri_plugin_host::sandbox::engine().expect("engine");
    let component = wasmtime::component::Component::from_file(&engine, component_path)
        .expect("sample component loads");
    let limits = kattri_plugins::sandbox::SandboxLimits::new(
        kattri_plugins::sandbox::DEFAULT_FUEL_PER_CALL,
        kattri_plugins::sandbox::DEFAULT_MEMORY_BYTES,
        kattri_plugins::sandbox::DEFAULT_WALL_CLOCK,
        kattri_plugins::sandbox::DEFAULT_MAX_OUTPUT_BYTES,
    )
    .expect("default limits are valid");
    let manifest = manifest(r#""presence.write","log.write""#, "");
    let state = host_with(manifest, &[Permission::PresenceWrite, Permission::LogWrite]);

    let mut loaded = kattri_plugin_host::world::load(&engine, &limits, &component, state)
        .expect("component instantiates");
    loaded
        .init("org.kattri.sample-presence")
        .expect("init succeeds");
}
