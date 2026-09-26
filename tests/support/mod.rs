use std::path::PathBuf;

pub fn adapter_command(mode: &str) -> Vec<String> {
    // Cargo builds example targets for `cargo test`. Locate the fixture beside
    // this profile's test executables, including custom target directories.
    let test_binary = std::env::current_exe().unwrap();
    let profile = test_binary.parent().unwrap().parent().unwrap();
    let fixture: PathBuf = profile
        .join("examples")
        .join(format!("test-adapter{}", std::env::consts::EXE_SUFFIX));

    assert!(
        fixture.is_file(),
        "build the fixture with cargo build --example test-adapter"
    );
    vec![fixture.to_string_lossy().into_owned(), mode.to_owned()]
}
