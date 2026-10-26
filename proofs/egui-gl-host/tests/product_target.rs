use std::path::Path;

#[test]
fn cargo_builds_the_existing_host_under_the_packaged_name() {
    let executable = Path::new(env!("CARGO_BIN_EXE_jellyfin-kattri"));

    assert!(
        executable.is_file(),
        "Cargo-provided product executable path does not exist: {}",
        executable.display()
    );
}
