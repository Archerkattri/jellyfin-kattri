//! The egui client must reach every user-facing string through the en-US catalog (M4-18).
//!
//! These tests read the client source as text. They check that each `ids::NAME` the client uses
//! is defined and present in `kattri.kv`, that no catalog entry is left unused, and that no
//! literal is passed to the lookup helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use kattri_i18n::english;

const IDS_SOURCE: &str = include_str!("../src/ids.rs");

fn client_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proofs/egui-client/src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn client_sources() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    rust_files(&client_src(), &mut files);
    files.sort();
    files
        .into_iter()
        .map(|file| {
            let text =
                fs::read_to_string(&file).unwrap_or_else(|err| panic!("{}: {err}", file.display()));
            (file, text)
        })
        .collect()
}

/// Constant names the client refers to, as written after `ids::`.
fn referenced_names(sources: &[(PathBuf, String)]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (_, text) in sources {
        for segment in text.split("ids::").skip(1) {
            let name: String = segment
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            if !name.is_empty() {
                names.insert(name);
            }
        }
    }
    names
}

/// Constant name to message id, read from `pub const NAME: &str = "id";` declarations. rustfmt
/// may wrap the value onto the next line, so the scan works on the whole source.
fn defined_ids() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for chunk in IDS_SOURCE.split("pub const ").skip(1) {
        let Some((name, rest)) = chunk.split_once(':') else {
            continue;
        };
        let Some(after_eq) = rest.trim_start().strip_prefix("&str") else {
            continue;
        };
        let Some(value) = after_eq.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(value) = value.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some((id, _)) = value.split_once('"') {
            map.insert(name.trim().to_owned(), id.to_owned());
        }
    }
    map
}

#[test]
fn every_id_the_client_uses_is_defined_and_in_en_us() {
    let sources = client_sources();
    let defined = defined_ids();
    let catalog = english().expect("shipped en-US catalog parses");
    let mut problems = Vec::new();
    for name in referenced_names(&sources) {
        match defined.get(&name) {
            None => problems.push(format!("ids::{name} is not defined in ids.rs")),
            Some(id) if catalog.get(id).is_none() => {
                problems.push(format!("ids::{name} = {id} is missing from kattri.kv"));
            }
            Some(_) => {}
        }
    }
    assert!(
        problems.is_empty(),
        "client ids missing from the catalog:\n{}",
        problems.join("\n")
    );
}

#[test]
fn en_us_catalog_has_no_id_the_client_does_not_use() {
    let sources = client_sources();
    let defined = defined_ids();
    let used: BTreeSet<&str> = referenced_names(&sources)
        .iter()
        .filter_map(|name| defined.get(name).map(String::as_str))
        .collect();
    let catalog = english().expect("shipped en-US catalog parses");
    let unused: Vec<&str> = catalog.ids().filter(|id| !used.contains(id)).collect();
    assert!(
        unused.is_empty(),
        "catalog ids the client never uses (remove them or wire them up):\n{}",
        unused.join("\n")
    );
}

#[test]
fn every_defined_id_is_listed_in_ids_all() {
    let defined = defined_ids();
    let all_start = IDS_SOURCE
        .find("pub const ALL")
        .expect("ids.rs declares ALL");
    let all_block = &IDS_SOURCE[all_start..];
    let missing: Vec<&String> = defined
        .keys()
        .filter(|name| !all_block.contains(&format!("    {name},")))
        .collect();
    assert!(
        missing.is_empty(),
        "constants not listed in ids::ALL: {missing:?}"
    );
}

#[test]
fn no_literal_is_passed_to_the_lookup_helpers() {
    let mut literal_calls = Vec::new();
    for (file, text) in client_sources() {
        for helper in ["ui_text(", "ui_text_args("] {
            for (index, line) in text.lines().enumerate() {
                if line.contains(helper) && line.contains(&format!("{helper}\"")) {
                    literal_calls.push(format!("{}:{}", file.display(), index + 1));
                }
            }
        }
    }
    assert!(
        literal_calls.is_empty(),
        "lookup helpers must take a message id constant, not a literal:\n{}",
        literal_calls.join("\n")
    );
}
