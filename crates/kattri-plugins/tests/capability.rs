//! M4-03: host capability model. Every spec capability name has a denied-without-grant
//! test and a granted test; revocation and cross-plugin tests cover per-plugin grants.

use kattri_plugins::{HostCall, Permission, PermissionGrants, PluginError, PluginManifest, invoke};
use serde_json::json;

/// A manifest declaring exactly `permissions` (spec capability names).
fn manifest_with(id: &str, permissions: &[&str]) -> PluginManifest {
    let text = json!({
        "id": id,
        "name": "Test Plugin",
        "version": "0.1.0",
        "api_version": 1,
        "entry": "main.wasm",
        "permissions": permissions,
    })
    .to_string();
    PluginManifest::from_json(&text).expect("test manifest is valid")
}

/// One call per spec capability, paired with the wire name it requires.
fn calls() -> Vec<(&'static str, HostCall)> {
    vec![
        ("playback.read", HostCall::PlaybackRead),
        (
            "presence.write",
            HostCall::PresenceWrite {
                payload: r#"{"state":"playing"}"#.to_owned(),
            },
        ),
        (
            "settings.own",
            HostCall::SettingsOwnRead {
                key: "skip_intro".to_owned(),
            },
        ),
        (
            "settings.own",
            HostCall::SettingsOwnWrite {
                key: "skip_intro".to_owned(),
                value: "true".to_owned(),
            },
        ),
        (
            "log.write",
            HostCall::LogWrite {
                line: "hello".to_owned(),
            },
        ),
    ]
}

fn denied(name: &str) -> PluginError {
    PluginError::PermissionDenied {
        permission: name.to_owned(),
    }
}

#[test]
fn presence_write_without_grant_returns_permission_denied() {
    // Acceptance: a plugin that requests presence.write but has no approved grant.
    let m = manifest_with("org.example.presence", &["presence.write"]);
    let grants = PermissionGrants::for_plugin(&m);
    let call = HostCall::PresenceWrite {
        payload: "{}".to_owned(),
    };
    assert_eq!(invoke(&m, &grants, &call), Err(denied("presence.write")));
}

#[test]
fn presence_write_with_grant_succeeds() {
    let m = manifest_with("org.example.presence", &["presence.write"]);
    let mut grants = PermissionGrants::for_plugin(&m);
    grants.grant(Permission::PresenceWrite);
    let call = HostCall::PresenceWrite {
        payload: "{}".to_owned(),
    };
    assert_eq!(invoke(&m, &grants, &call), Ok(()));
}

#[test]
fn every_capability_name_is_denied_without_a_grant() {
    for (name, call) in calls() {
        let m = manifest_with("org.example.all", &[name]);
        let grants = PermissionGrants::for_plugin(&m);
        assert_eq!(
            invoke(&m, &grants, &call),
            Err(denied(name)),
            "capability {name}"
        );
    }
}

#[test]
fn every_capability_name_is_allowed_once_granted() {
    for (name, call) in calls() {
        let m = manifest_with("org.example.all", &[name]);
        let mut grants = PermissionGrants::for_plugin(&m);
        grants.grant(call.required_permission());
        assert_eq!(invoke(&m, &grants, &call), Ok(()), "capability {name}");
    }
}

#[test]
fn every_capability_name_parses_from_a_manifest_and_matches_its_call() {
    for name in [
        "playback.read",
        "presence.write",
        "settings.own",
        "log.write",
    ] {
        let m = manifest_with("org.example.names", &[name]);
        assert_eq!(m.permissions.len(), 1, "{name} parses");
        assert_eq!(m.permissions[0].wire_name(), name);
        assert!(m.permissions[0].requires_consent(), "{name} needs a grant");
    }
    let unknown = json!({
        "id": "org.example.names",
        "name": "T",
        "version": "0.1.0",
        "api_version": 1,
        "entry": "main.wasm",
        "permissions": ["filesystem.read"],
    })
    .to_string();
    assert!(matches!(
        PluginManifest::from_json(&unknown),
        Err(PluginError::Json(msg)) if msg.contains("filesystem.read")
    ));
}

#[test]
fn undeclared_capability_is_denied_even_when_granted() {
    let declared = manifest_with("org.example.undeclared", &["log.write"]);
    let mut grants = PermissionGrants::for_plugin(&declared);
    grants.grant(Permission::PresenceWrite);
    let call = HostCall::PresenceWrite {
        payload: "{}".to_owned(),
    };
    assert_eq!(
        invoke(&declared, &grants, &call),
        Err(denied("presence.write"))
    );
}

#[test]
fn revoking_a_grant_fails_the_next_call_not_the_running_plugin() {
    let m = manifest_with("org.example.revoke", &["presence.write"]);
    let mut grants = PermissionGrants::for_plugin(&m);
    grants.grant(Permission::PresenceWrite);
    let call = HostCall::PresenceWrite {
        payload: "{}".to_owned(),
    };
    assert_eq!(invoke(&m, &grants, &call), Ok(()));
    grants.revoke(&Permission::PresenceWrite);
    assert_eq!(invoke(&m, &grants, &call), Err(denied("presence.write")));
    // The manifest and the grant set are still usable: re-granting restores access.
    grants.grant(Permission::PresenceWrite);
    assert_eq!(invoke(&m, &grants, &call), Ok(()));
}

#[test]
fn grants_are_per_plugin_and_never_shared() {
    let a = manifest_with("org.example.a", &["presence.write"]);
    let b = manifest_with("org.example.b", &["presence.write"]);
    let mut grants_a = PermissionGrants::for_plugin(&a);
    grants_a.grant(Permission::PresenceWrite);
    let call = HostCall::PresenceWrite {
        payload: "{}".to_owned(),
    };
    assert_eq!(invoke(&a, &grants_a, &call), Ok(()));
    // A's grant set must not authorize B, even though B declares the same capability.
    assert_eq!(invoke(&b, &grants_a, &call), Err(denied("presence.write")));
    // B has no grants of its own.
    let grants_b = PermissionGrants::for_plugin(&b);
    assert_eq!(invoke(&b, &grants_b, &call), Err(denied("presence.write")));
}

#[test]
fn unbound_grant_set_authorizes_nothing_on_the_host_path() {
    let m = manifest_with("org.example.unbound", &["presence.write"]);
    let mut grants = PermissionGrants::new();
    grants.grant(Permission::PresenceWrite);
    let call = HostCall::PresenceWrite {
        payload: "{}".to_owned(),
    };
    assert_eq!(invoke(&m, &grants, &call), Err(denied("presence.write")));
}

#[test]
fn no_grant_is_implicit_for_any_capability_call_site() {
    // Every host call the model exposes is refused when nothing is granted.
    let m_all = manifest_with(
        "org.example.implicit",
        &[
            "playback.read",
            "presence.write",
            "settings.own",
            "log.write",
        ],
    );
    let grants = PermissionGrants::for_plugin(&m_all);
    for (name, call) in calls() {
        assert_eq!(
            invoke(&m_all, &grants, &call),
            Err(denied(name)),
            "call site {name} must not be implicit"
        );
    }
}

#[test]
fn granting_one_capability_does_not_grant_another() {
    let m = manifest_with(
        "org.example.one",
        &[
            "playback.read",
            "presence.write",
            "settings.own",
            "log.write",
        ],
    );
    let mut grants = PermissionGrants::for_plugin(&m);
    grants.grant(Permission::LogWrite);
    assert_eq!(
        invoke(&m, &grants, &HostCall::PlaybackRead),
        Err(denied("playback.read"))
    );
    assert_eq!(
        invoke(
            &m,
            &grants,
            &HostCall::PresenceWrite {
                payload: "{}".to_owned()
            }
        ),
        Err(denied("presence.write"))
    );
    assert_eq!(
        invoke(
            &m,
            &grants,
            &HostCall::SettingsOwnRead {
                key: "k".to_owned()
            }
        ),
        Err(denied("settings.own"))
    );
    assert_eq!(
        invoke(
            &m,
            &grants,
            &HostCall::LogWrite {
                line: "x".to_owned()
            }
        ),
        Ok(())
    );
}

#[test]
fn log_write_is_length_capped_after_authorization() {
    let m = manifest_with("org.example.log", &["log.write"]);
    let mut grants = PermissionGrants::for_plugin(&m);
    grants.grant(Permission::LogWrite);
    let at_cap = HostCall::LogWrite {
        line: "x".repeat(512),
    };
    assert_eq!(invoke(&m, &grants, &at_cap), Ok(()));
    let over = HostCall::LogWrite {
        line: "x".repeat(513),
    };
    assert!(matches!(
        invoke(&m, &grants, &over),
        Err(PluginError::Invalid {
            field: "log.write",
            ..
        })
    ));
}

#[test]
fn settings_keys_are_bounded_after_authorization() {
    let m = manifest_with("org.example.settings", &["settings.own"]);
    let mut grants = PermissionGrants::for_plugin(&m);
    grants.grant(Permission::SettingsOwn);
    let empty = HostCall::SettingsOwnRead { key: String::new() };
    assert!(matches!(
        invoke(&m, &grants, &empty),
        Err(PluginError::Invalid {
            field: "settings.own",
            ..
        })
    ));
    let long = HostCall::SettingsOwnWrite {
        key: "k".repeat(129),
        value: "v".to_owned(),
    };
    assert!(matches!(
        invoke(&m, &grants, &long),
        Err(PluginError::Invalid {
            field: "settings.own",
            ..
        })
    ));
}

#[test]
fn denied_call_does_not_reveal_payload_rules() {
    // Authorization runs first: an oversized line from an unauthorized plugin is a
    // PermissionDenied, not a validation error.
    let m = manifest_with("org.example.order", &["log.write"]);
    let grants = PermissionGrants::for_plugin(&m);
    let over = HostCall::LogWrite {
        line: "x".repeat(10_000),
    };
    assert_eq!(invoke(&m, &grants, &over), Err(denied("log.write")));
}
