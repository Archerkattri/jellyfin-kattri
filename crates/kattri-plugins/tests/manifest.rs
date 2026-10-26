//! M4-02: fail-closed manifest validation. Negative table, file-path resolution,
//! and a seeded fuzz loop that must never panic and only accepts manifests that
//! satisfy every invariant.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use kattri_plugins::manifest::{MAX_MANIFEST_BYTES, ensure_id_available, resolve_entry};
use kattri_plugins::{PLUGIN_API_VERSION, PluginError, PluginManifest};
use serde_json::{Map, Value, json};

fn base_value() -> Value {
    json!({
        "id": "org.example.skip-credits",
        "name": "Skip Credits",
        "version": "1.2.3",
        "api_version": 1,
        "entry": "dist/main.wasm",
        "permissions": ["read_playback", "read_library"]
    })
}

fn base_text() -> String {
    base_value().to_string()
}

/// The base manifest with one top-level change applied.
fn with(change: impl FnOnce(&mut Map<String, Value>)) -> String {
    let mut value = base_value();
    change(value.as_object_mut().expect("base is an object"));
    value.to_string()
}

#[derive(Debug, Clone, Copy)]
enum Expect {
    /// `Invalid` with this field name.
    Field(&'static str),
    /// `Json` error whose message contains this text (names the offending key or value).
    Json(&'static str),
    /// `Json` error of any kind (wrong type, wrong shape).
    AnyJson,
    /// `UnsupportedApi`.
    Api,
}

fn assert_rejected(label: &str, text: &str, expect: Expect) {
    let result = PluginManifest::from_json(text);
    let ok = match (expect, &result) {
        (Expect::Field(f), Err(PluginError::Invalid { field, .. })) => *field == f,
        (Expect::Json(needle), Err(PluginError::Json(msg))) => msg.contains(needle),
        (Expect::AnyJson, Err(PluginError::Json(_))) => true,
        (Expect::Api, Err(PluginError::UnsupportedApi { .. })) => true,
        _ => false,
    };
    assert!(ok, "{label}: expected {expect:?}, got {result:?}");
}

#[test]
fn valid_manifests_are_accepted() {
    PluginManifest::from_json(&base_text()).expect("base manifest is valid");
    let with_optional = with(|m| {
        m.insert("min_host_version".into(), json!("0.4.0-rc.1"));
        m.insert("description".into(), json!("Line one\nLine two"));
        m.insert("name".into(), json!("Skip Crédits"));
    });
    let parsed = PluginManifest::from_json(&with_optional).expect("optional fields are valid");
    assert_eq!(parsed.min_host_version.as_deref(), Some("0.4.0-rc.1"));
    let no_permissions = with(|m| {
        m.remove("permissions");
    });
    assert!(PluginManifest::from_json(&no_permissions).is_ok());
}

#[test]
fn negative_manifest_table_rejects_every_case() {
    let long_id = format!("org.{}", "a".repeat(61)); // 65 characters
    let cases: Vec<(&str, String, Expect)> = vec![
        (
            "missing id",
            with(|m| {
                m.remove("id");
            }),
            Expect::Json("`id`"),
        ),
        (
            "missing name",
            with(|m| {
                m.remove("name");
            }),
            Expect::Json("`name`"),
        ),
        (
            "missing version",
            with(|m| {
                m.remove("version");
            }),
            Expect::Json("`version`"),
        ),
        (
            "missing api_version",
            with(|m| {
                m.remove("api_version");
            }),
            Expect::Json("`api_version`"),
        ),
        (
            "missing entry",
            with(|m| {
                m.remove("entry");
            }),
            Expect::Json("`entry`"),
        ),
        (
            "unknown top-level key",
            with(|m| {
                m.insert("homepage".into(), json!("x"));
            }),
            Expect::Json("`homepage`"),
        ),
        (
            "misspelled permissions key",
            with(|m| {
                let v = m.remove("permissions").expect("present");
                m.insert("permisions".into(), v);
            }),
            Expect::Json("`permisions`"),
        ),
        (
            "api_version as string",
            with(|m| {
                m.insert("api_version".into(), json!("1"));
            }),
            Expect::AnyJson,
        ),
        (
            "api_version as float",
            with(|m| {
                m.insert("api_version".into(), json!(1.5));
            }),
            Expect::AnyJson,
        ),
        (
            "api_version negative",
            with(|m| {
                m.insert("api_version".into(), json!(-1));
            }),
            Expect::AnyJson,
        ),
        (
            "api_version unsupported",
            with(|m| {
                m.insert("api_version".into(), json!(PLUGIN_API_VERSION + 1));
            }),
            Expect::Api,
        ),
        (
            "name as number",
            with(|m| {
                m.insert("name".into(), json!(42));
            }),
            Expect::AnyJson,
        ),
        (
            "permissions as string",
            with(|m| {
                m.insert("permissions".into(), json!("playback.read"));
            }),
            Expect::AnyJson,
        ),
        (
            "permission not in allowlist",
            with(|m| {
                m.insert("permissions".into(), json!(["root_access"]));
            }),
            Expect::Json("root_access"),
        ),
        (
            "one unknown permission among valid",
            with(|m| {
                m.insert(
                    "permissions".into(),
                    json!(["read_playback", "filesystem.read"]),
                );
            }),
            Expect::Json("filesystem.read"),
        ),
        (
            "permission wrong type",
            with(|m| {
                m.insert("permissions".into(), json!([42]));
            }),
            Expect::AnyJson,
        ),
        (
            "network wildcard host",
            with(|m| {
                m.insert(
                    "permissions".into(),
                    json!([{"network": {"host": "*.example.org"}}]),
                );
            }),
            Expect::Field("permissions.network.host"),
        ),
        (
            "duplicate permission",
            with(|m| {
                m.insert(
                    "permissions".into(),
                    json!(["read_library", "read_library"]),
                );
            }),
            Expect::Field("permissions"),
        ),
        (
            "too many permissions",
            with(|m| {
                m.insert("permissions".into(), json!(vec!["read_library"; 33]));
            }),
            Expect::Field("permissions"),
        ),
        (
            "id longer than 64",
            with(|m| {
                m.insert("id".into(), json!(long_id));
            }),
            Expect::Field("id"),
        ),
        (
            "id uppercase",
            with(|m| {
                m.insert("id".into(), json!("Org.Example.app"));
            }),
            Expect::Field("id"),
        ),
        (
            "id single segment",
            with(|m| {
                m.insert("id".into(), json!("ab"));
            }),
            Expect::Field("id"),
        ),
        (
            "name longer than 80",
            with(|m| {
                m.insert("name".into(), json!("n".repeat(81)));
            }),
            Expect::Field("name"),
        ),
        (
            "name with control character",
            with(|m| {
                m.insert("name".into(), json!("Skip\u{7}"));
            }),
            Expect::Field("name"),
        ),
        (
            "name blank",
            with(|m| {
                m.insert("name".into(), json!("   "));
            }),
            Expect::Field("name"),
        ),
        (
            "version not semver",
            with(|m| {
                m.insert("version".into(), json!("1.2"));
            }),
            Expect::Field("version"),
        ),
        (
            "version leading zero",
            with(|m| {
                m.insert("version".into(), json!("01.2.3"));
            }),
            Expect::Field("version"),
        ),
        (
            "version longer than 64",
            with(|m| {
                m.insert("version".into(), json!(format!("1.0.0-{}", "a".repeat(70))));
            }),
            Expect::Field("version"),
        ),
        (
            "min_host_version not semver",
            with(|m| {
                m.insert("min_host_version".into(), json!("soon"));
            }),
            Expect::Field("min_host_version"),
        ),
        (
            "description longer than 1000",
            with(|m| {
                m.insert("description".into(), json!("d".repeat(1001)));
            }),
            Expect::Field("description"),
        ),
        (
            "description with NUL",
            with(|m| {
                m.insert("description".into(), json!("a\u{0}b"));
            }),
            Expect::Field("description"),
        ),
        (
            "entry parent traversal",
            with(|m| {
                m.insert("entry".into(), json!("../evil.wasm"));
            }),
            Expect::Field("entry"),
        ),
        (
            "entry absolute",
            with(|m| {
                m.insert("entry".into(), json!("/etc/x.wasm"));
            }),
            Expect::Field("entry"),
        ),
        (
            "entry drive prefix",
            with(|m| {
                m.insert("entry".into(), json!("C:\\x.wasm"));
            }),
            Expect::Field("entry"),
        ),
        (
            "entry backslash traversal",
            with(|m| {
                m.insert("entry".into(), json!("..\\..\\x.wasm"));
            }),
            Expect::Field("entry"),
        ),
        (
            "entry dot segment",
            with(|m| {
                m.insert("entry".into(), json!("./main.wasm"));
            }),
            Expect::Field("entry"),
        ),
        (
            "entry empty",
            with(|m| {
                m.insert("entry".into(), json!(""));
            }),
            Expect::Field("entry"),
        ),
        (
            "entry longer than 255",
            with(|m| {
                m.insert("entry".into(), json!(format!("{}.wasm", "a".repeat(256))));
            }),
            Expect::Field("entry"),
        ),
        (
            "entry with NUL",
            with(|m| {
                m.insert("entry".into(), json!("a\u{0}b.wasm"));
            }),
            Expect::Field("entry"),
        ),
        (
            "entry trailing slash",
            with(|m| {
                m.insert("entry".into(), json!("dist/"));
            }),
            Expect::Field("entry"),
        ),
        (
            "entry double slash",
            with(|m| {
                m.insert("entry".into(), json!("dist//x.wasm"));
            }),
            Expect::Field("entry"),
        ),
        (
            "trailing content after object",
            format!("{} x", base_text()),
            Expect::AnyJson,
        ),
        ("top-level array", "[]".to_owned(), Expect::AnyJson),
        ("empty document", String::new(), Expect::AnyJson),
    ];
    assert!(
        cases.len() >= 15,
        "table must hold at least 15 negative cases"
    );
    for (label, text, expect) in &cases {
        assert_rejected(label, text, *expect);
    }
}

#[test]
fn oversized_manifest_is_rejected_before_parsing() {
    let padded = format!("{}{}", " ".repeat(MAX_MANIFEST_BYTES), base_text());
    assert_rejected("oversized json", &padded, Expect::Field("manifest"));
    let err = PluginManifest::from_bytes(padded.as_bytes());
    assert!(matches!(
        err,
        Err(PluginError::Invalid {
            field: "manifest",
            ..
        })
    ));
}

#[test]
fn non_utf8_bytes_are_rejected() {
    let mut bytes = base_text().into_bytes();
    let idx = bytes.iter().position(|&b| b == b'S').expect("name present");
    bytes[idx] = 0xFF;
    assert!(matches!(
        PluginManifest::from_bytes(&bytes),
        Err(PluginError::Invalid {
            field: "manifest",
            ..
        })
    ));
}

#[test]
fn duplicate_plugin_id_is_rejected_against_loaded_set() {
    let loaded = PluginManifest::from_json(&base_text()).expect("valid");
    let same_id = PluginManifest::from_json(&with(|m| {
        m.insert("name".into(), json!("Another"));
    }))
    .expect("valid");
    assert_eq!(
        ensure_id_available(std::slice::from_ref(&loaded), &same_id),
        Err(PluginError::DuplicateId {
            id: "org.example.skip-credits".into()
        })
    );
    let other = PluginManifest::from_json(&with(|m| {
        m.insert("id".into(), json!("org.example.other"));
    }))
    .expect("valid");
    assert!(ensure_id_available(std::slice::from_ref(&loaded), &other).is_ok());
}

/// A fresh directory under the OS temp dir, unique per test label and process.
fn scratch_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kattri-plugins-m4-02-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn manifest_with_entry(entry: &str) -> PluginManifest {
    PluginManifest::from_json(&with(|m| {
        m.insert("entry".into(), json!(entry));
    }))
    .expect("entry is valid by shape")
}

#[test]
fn resolve_entry_accepts_a_file_inside_the_folder() {
    let dir = scratch_dir("inside");
    std::fs::create_dir_all(dir.join("dist")).expect("mkdir");
    std::fs::write(dir.join("dist").join("main.wasm"), b"\0asm").expect("write");
    let resolved = resolve_entry(&dir, &manifest_with_entry("dist/main.wasm")).expect("resolves");
    assert!(resolved.starts_with(dir.canonicalize().expect("canon")));
    let windows_style = resolve_entry(&dir, &manifest_with_entry("dist\\main.wasm"));
    assert!(
        windows_style.is_ok(),
        "backslash form normalizes to the same file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_entry_rejects_missing_and_directory_entries() {
    let dir = scratch_dir("missing");
    std::fs::create_dir_all(dir.join("dist")).expect("mkdir");
    assert!(matches!(
        resolve_entry(&dir, &manifest_with_entry("nope.wasm")),
        Err(PluginError::Invalid { field: "entry", .. })
    ));
    assert!(matches!(
        resolve_entry(&dir, &manifest_with_entry("dist")),
        Err(PluginError::Invalid { field: "entry", .. })
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn resolve_entry_rejects_a_symlink_that_escapes_the_folder() {
    let root = scratch_dir("symlink");
    let plugin = root.join("plugin");
    std::fs::create_dir_all(&plugin).expect("mkdir");
    let outside = root.join("secret.wasm");
    std::fs::write(&outside, b"secret").expect("write");
    std::os::unix::fs::symlink(&outside, plugin.join("entry.wasm")).expect("symlink");
    let result = resolve_entry(&plugin, &manifest_with_entry("entry.wasm"));
    assert!(
        matches!(result, Err(PluginError::Invalid { field: "entry", .. })),
        "symlink escape must be rejected, got {result:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Deterministic xorshift64 generator. Same seed, same corpus, every run.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn byte(&mut self) -> u8 {
        self.next_u64() as u8
    }
}

const FRAGMENTS: &[&str] = &[
    "a",
    "z",
    "0",
    "9",
    "-",
    ".",
    "/",
    "\\",
    ":",
    " ",
    "\"",
    "{",
    "}",
    "[",
    "]",
    ",",
    "\\u0000",
    "\\u0007",
    "é",
    "\u{1F600}",
    "..",
    "1.2.3",
    "org.x.y",
    "dist/main.wasm",
    "read_playback",
];

fn random_string(rng: &mut Rng) -> String {
    let parts = rng.below(12);
    (0..parts)
        .map(|_| FRAGMENTS[rng.below(FRAGMENTS.len())])
        .collect()
}

fn random_json(rng: &mut Rng, depth: u32) -> Value {
    let choice = if depth == 0 {
        rng.below(5)
    } else {
        rng.below(7)
    };
    match choice {
        0 => Value::Null,
        1 => Value::Bool(rng.below(2) == 0),
        2 => json!(rng.next_u64() as i64),
        3 => json!((rng.next_u64() as f64) / 1e9),
        4 => Value::String(random_string(rng)),
        5 => Value::Array(
            (0..rng.below(4))
                .map(|_| random_json(rng, depth - 1))
                .collect(),
        ),
        _ => {
            let mut map = Map::new();
            for _ in 0..rng.below(3) {
                map.insert(random_string(rng), random_json(rng, depth - 1));
            }
            Value::Object(map)
        }
    }
}

/// Every accepted manifest must satisfy all rules, not just the ones the mutation hit.
fn assert_invariants(m: &PluginManifest) {
    m.validate().expect("accepted manifest re-validates");
    assert_eq!(m.api_version, PLUGIN_API_VERSION);

    assert!((3..=64).contains(&m.id.len()), "id length {:?}", m.id);
    let segments: Vec<&str> = m.id.split('.').collect();
    assert!(segments.len() >= 2);
    for segment in segments {
        assert!(!segment.is_empty() && !segment.starts_with('-') && !segment.ends_with('-'));
        assert!(
            segment
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "id segment {segment:?}"
        );
    }

    assert!(!m.name.trim().is_empty());
    assert!(m.name.chars().count() <= 80);
    assert!(!m.name.chars().any(char::is_control));

    semver::Version::parse(&m.version).expect("version is semver");
    if let Some(min) = &m.min_host_version {
        semver::Version::parse(min).expect("min_host_version is semver");
    }

    assert!(m.description.chars().count() <= 1000);
    assert!(
        !m.description
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    );

    assert!(!m.entry.contains(':'));
    assert!(!m.entry.starts_with('/') && !m.entry.starts_with('\\'));
    for segment in m.entry.split(['/', '\\']) {
        assert!(
            !segment.is_empty() && segment != "." && segment != "..",
            "entry {:?}",
            m.entry
        );
    }

    assert!(m.permissions.len() <= 32);
    let unique: HashSet<_> = m.permissions.iter().collect();
    assert_eq!(
        unique.len(),
        m.permissions.len(),
        "permissions must be unique"
    );

    let text = serde_json::to_string(m).expect("serialize accepted manifest");
    let again = PluginManifest::from_json(&text).expect("round trip re-accepts");
    assert_eq!(&again, m, "round trip is lossless");
}

#[test]
fn fuzz_random_bytes_never_panic_and_accepted_manifests_hold_invariants() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut accepted = 0usize;
    for _ in 0..5_000 {
        let len = rng.below(512);
        let bytes: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        if let Ok(m) = PluginManifest::from_bytes(&bytes) {
            accepted += 1;
            assert_invariants(&m);
        }
    }
    assert_eq!(
        accepted, 0,
        "random bytes should never form a valid manifest"
    );
}

#[test]
fn fuzz_byte_mutations_of_a_valid_manifest_never_panic() {
    let seed = base_text().into_bytes();
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut accepted = 0usize;
    for _ in 0..20_000 {
        let mut bytes = seed.clone();
        for _ in 0..=rng.below(4) {
            if bytes.is_empty() {
                break;
            }
            let idx = rng.below(bytes.len());
            match rng.below(4) {
                0 => bytes[idx] = rng.byte(),
                1 => bytes.insert(idx, rng.byte()),
                2 => {
                    bytes.remove(idx);
                }
                _ => bytes.truncate(idx),
            }
        }
        if let Ok(m) = PluginManifest::from_bytes(&bytes) {
            accepted += 1;
            assert_invariants(&m);
        }
    }
    assert!(
        accepted > 0,
        "some benign mutations should still be accepted"
    );
}

#[test]
fn fuzz_json_level_mutations_never_panic_and_accepted_manifests_hold_invariants() {
    const KEYS: &[&str] = &[
        "id",
        "name",
        "version",
        "api_version",
        "entry",
        "permissions",
        "description",
        "min_host_version",
        "extra",
    ];
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    let mut accepted = 0usize;
    for _ in 0..20_000 {
        let mut value = base_value();
        let map = value.as_object_mut().expect("object");
        for _ in 0..=rng.below(3) {
            let key = KEYS[rng.below(KEYS.len())];
            if rng.below(4) == 0 {
                map.remove(key);
            } else {
                map.insert(key.to_owned(), random_json(&mut rng, 2));
            }
        }
        let text = value.to_string();
        if let Ok(m) = PluginManifest::from_json(&text) {
            accepted += 1;
            assert_invariants(&m);
        }
    }
    assert!(
        accepted > 0,
        "some benign mutations should still be accepted"
    );
}

#[test]
fn accepted_path_helpers_do_not_escape_for_fuzzed_entries() {
    let mut rng = Rng(0x0BAD_5EED_0000_0001);
    for _ in 0..5_000 {
        let entry = random_string(&mut rng);
        let text = with(|m| {
            m.insert("entry".into(), json!(entry));
        });
        if let Ok(m) = PluginManifest::from_json(&text) {
            // Whatever passed validation must not be able to climb out of the folder
            // when joined onto a base path.
            let base = Path::new("base-dir");
            let joined: PathBuf = m
                .entry
                .split(['/', '\\'])
                .fold(base.to_path_buf(), |p, s| p.join(s));
            assert!(joined.starts_with(base));
            assert!(!m.entry.split(['/', '\\']).any(|s| s == ".."));
        }
    }
}
