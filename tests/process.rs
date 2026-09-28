//! Process-boundary tests using the native Rust adapter fixture.
mod support;

use object_tests::{model::*, runner};
use serde_json::json;
use support::adapter_command;

fn load_vector_case() -> (Case, Profile) {
    let suite = Suite::load(concat!(env!("CARGO_MANIFEST_DIR"), "/cases/vectors.json")).unwrap();
    (suite.cases[0].clone(), suite.profiles["azure"].clone())
}

#[test]
fn malformed_output_is_failed_not_unsupported() {
    let (case, profile) = load_vector_case();
    let report =
        runner::grade_case(&case, &profile, &adapter_command("malformed-output"), None).unwrap();
    assert_eq!(report["verdict"], "failed");
}

#[test]
fn exit_failure_does_not_pass_even_with_correct_output() {
    let (case, profile) = load_vector_case();
    let report =
        runner::grade_case(&case, &profile, &adapter_command("unsuccessful-exit"), None).unwrap();
    assert_eq!(report["verdict"], "failed");
}

#[test]
fn adapter_sees_inputs_but_no_expected_answers() {
    let (case, profile) = load_vector_case();
    assert_eq!(
        runner::grade_case(&case, &profile, &adapter_command("check-input"), None).unwrap()["verdict"],
        "unsupported"
    );
}

#[test]
fn two_json_answers_are_not_accepted() {
    let (case, profile) = load_vector_case();
    assert_eq!(
        runner::grade_case(&case, &profile, &adapter_command("multiple-results"), None).unwrap()["verdict"],
        "failed"
    );
}

#[test]
#[ignore = "requires permission to bind a loopback port"]
fn real_http_exchange_and_head_length() {
    let suite = Suite::load(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases.json")).unwrap();
    for (id, object_size) in [
        ("azure/get-key-plain", 5),
        ("azure/head-size", 123),
        ("azure/head-size", 32768),
        ("azure/head-size", 5242886),
    ] {
        let mut case = suite
            .cases
            .iter()
            .find(|case| case.id == id)
            .unwrap()
            .clone();
        if id == "azure/head-size" {
            let response = &mut case.exchanges[0].alternatives[0].response;
            response.headers.remove("content-length");
            response.headers.insert(
                "Content-Length".into(),
                Header::Literal(object_size.to_string()),
            );
            let size_check = case
                .expect
                .iter_mut()
                .find(|check| check.at == "/value/size")
                .unwrap();
            size_check.rule = Rule::Equal {
                value: json!(object_size),
            };
        }
        let profile = &suite.profiles["azure"];
        let report = runner::grade_case(&case, profile, &adapter_command("http"), None).unwrap();
        assert_eq!(report["verdict"], json!("pass"), "{report}");
    }
}

#[test]
fn non_object_output_is_a_protocol_failure() {
    let (case, profile) = load_vector_case();
    let report =
        runner::grade_case(&case, &profile, &adapter_command("non-object-result"), None).unwrap();
    assert_eq!(report["verdict"], "failed");
}

// A recorded list keeps its hand-written patterns and lists only the cases they
// miss, and a later run held to that list passes.
#[test]
fn recording_keeps_patterns_and_a_run_then_meets_the_list() {
    let list = std::env::temp_dir().join(format!("object-tests-list-{}.json", std::process::id()));
    std::fs::write(&list, r#"{"vectors": {"azure/*": "no Azure"}}"#).unwrap();
    let grade = |list_option: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_object-tests"))
            .args([
                "grade",
                concat!(env!("CARGO_MANIFEST_DIR"), "/cases/vectors.json"),
                list_option,
            ])
            .arg(&list)
            .arg("--")
            .args(adapter_command("check-input"))
            .output()
            .unwrap()
    };

    assert!(grade("--record-unsupported").status.success());
    let recorded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&list).unwrap()).unwrap();
    let entries = recorded["vectors"].as_object().unwrap();
    assert!(entries.contains_key("azure/*"));
    assert!(entries.len() > 1);
    assert!(
        entries
            .keys()
            .all(|entry| entry == "azure/*" || !entry.starts_with("azure/"))
    );

    assert!(grade("--expected-unsupported").status.success());
    std::fs::remove_file(&list).unwrap();
}
