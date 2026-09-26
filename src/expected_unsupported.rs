//! A client's own list of the cases it expects to report unsupported.
//!
//! The list maps a suite name to entries, and each entry to the reason its cases
//! are unsupported. A suite name is the suite file's name without `.json`. An
//! entry is a case ID, or a pattern in which `*` matches any run of characters,
//! such as `operations/s3/*` for every S3 case of the operations suite.
//!
//! A run graded against the list passes only if every case an entry covers is
//! unsupported and every other case passes. A regression fails the run, and so
//! does a case that the client now supports. An entry that covers no case of
//! its suite is an error.
use crate::{Result, model::Suite};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

/// Entries and the reason their cases are unsupported, by suite name.
pub type ExpectedUnsupportedCases = BTreeMap<String, BTreeMap<String, String>>;

/// Returns whether a list entry covers a case: the entry is the case's ID, or a
/// pattern whose `*` wildcards match any run of characters in it.
pub fn entry_covers(entry: &str, case_id: &str) -> bool {
    let mut parts = entry.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = case_id.strip_prefix(first) else {
        return false;
    };
    let middle_and_last: Vec<&str> = parts.collect();
    let Some((last, middle)) = middle_and_last.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(start) => rest = &rest[start + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

fn is_pattern(entry: &str) -> bool {
    entry.contains('*')
}

fn is_listed(listed_cases: &BTreeMap<String, String>, case_id: &str) -> bool {
    listed_cases
        .keys()
        .any(|entry| entry_covers(entry, case_id))
}

/// The name a suite's section has in the list: its file name without `.json`.
///
/// # Errors
/// Returns an error if the path has no UTF-8 file name.
pub fn suite_name(suite_path: &str) -> Result<String> {
    Ok(Path::new(suite_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("no suite name in {suite_path}"))?
        .to_owned())
}

/// Reads a list, or an empty one when `must_exist` is false and there is no file.
///
/// # Errors
/// Returns an error if the file cannot be read or is not a list.
pub fn load_expected_unsupported_cases(
    path: &str,
    must_exist: bool,
) -> Result<ExpectedUnsupportedCases> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if !must_exist && error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ExpectedUnsupportedCases::new())
        }
        Err(error) => Err(format!("{path}: {error}").into()),
    }
}

/// Returns the entries that cover no case of the suite.
pub fn unknown_listed_case_ids<'a>(
    listed_cases: &'a BTreeMap<String, String>,
    suite: &Suite,
) -> Vec<&'a str> {
    listed_cases
        .keys()
        .filter(|entry| !suite.cases.iter().any(|case| entry_covers(entry, &case.id)))
        .map(String::as_str)
        .collect()
}

/// Returns a mismatch for every graded case whose verdict differs from the
/// expected one. A case that an entry covers must be `unsupported`, and any other
/// case must pass.
pub fn find_verdict_mismatches(
    listed_cases: &BTreeMap<String, String>,
    graded_verdicts: &[(String, String)],
) -> Vec<Value> {
    graded_verdicts
        .iter()
        .filter_map(|(case_id, verdict)| {
            let expected_verdict = if is_listed(listed_cases, case_id) {
                "unsupported"
            } else {
                "pass"
            };
            (verdict != expected_verdict).then(|| {
                json!({
                    "id": case_id,
                    "expected": expected_verdict,
                    "verdict": verdict,
                })
            })
        })
        .collect()
}

/// Returns the reason that a report gives for an unsupported verdict.
pub fn unsupported_reason(report: &Value) -> String {
    match &report["adapter_note"] {
        Value::String(reason) => reason.clone(),
        Value::Array(reasons) => reasons
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("; "),
        _ => "unsupported".to_owned(),
    }
}

/// Replaces the case IDs of the graded cases with the ones graded unsupported,
/// keeping entries for cases this run did not grade. Patterns are written by
/// hand and kept: a case that a pattern covers gets no entry of its own.
pub fn record_unsupported_cases(
    listed_cases: &mut BTreeMap<String, String>,
    graded_reports: &[Value],
) {
    for report in graded_reports {
        let Some(case_id) = report["id"].as_str() else {
            continue;
        };
        let covered_by_pattern = listed_cases
            .keys()
            .any(|entry| is_pattern(entry) && entry_covers(entry, case_id));
        if report["verdict"] == "unsupported" && !covered_by_pattern {
            listed_cases.insert(case_id.to_owned(), unsupported_reason(report));
        } else {
            listed_cases.remove(case_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(case_ids: &[&str]) -> BTreeMap<String, String> {
        case_ids
            .iter()
            .map(|case_id| (case_id.to_string(), "reason".to_owned()))
            .collect()
    }

    fn graded(verdicts: &[(&str, &str)]) -> Vec<(String, String)> {
        verdicts
            .iter()
            .map(|(case_id, verdict)| (case_id.to_string(), verdict.to_string()))
            .collect()
    }

    #[test]
    fn listed_unsupported_and_unlisted_passes_match() {
        let mismatches = find_verdict_mismatches(
            &listed(&["snapshot"]),
            &graded(&[("snapshot", "unsupported"), ("get", "pass")]),
        );
        assert!(mismatches.is_empty());
    }

    #[test]
    fn regressions_new_support_and_wrong_answers_mismatch() {
        let mismatches = find_verdict_mismatches(
            &listed(&["snapshot", "version"]),
            &graded(&[
                ("get", "unsupported"),
                ("snapshot", "pass"),
                ("version", "wrong"),
                ("head", "failed"),
            ]),
        );
        let mismatched_ids: Vec<&str> = mismatches
            .iter()
            .map(|mismatch| mismatch["id"].as_str().unwrap())
            .collect();
        assert_eq!(mismatched_ids, ["get", "snapshot", "version", "head"]);
        assert_eq!(mismatches[1]["expected"], "unsupported");
        assert_eq!(mismatches[1]["verdict"], "pass");
    }

    #[test]
    fn a_pattern_covers_every_case_its_wildcards_match() {
        assert!(entry_covers("operations/s3/get", "operations/s3/get"));
        assert!(!entry_covers(
            "operations/s3/get",
            "operations/s3/get-whole"
        ));
        assert!(entry_covers("operations/s3/*", "operations/s3/get-whole"));
        assert!(!entry_covers(
            "operations/s3/*",
            "operations/s3-express/get-whole"
        ));
        assert!(entry_covers(
            "operations/azure*/lease-*",
            "operations/azure-hns/lease-break"
        ));
        assert!(!entry_covers(
            "operations/azure*/lease-*",
            "operations/azure/put-leased"
        ));
        assert!(entry_covers(
            "*-key-*",
            "operations/s3/put-created-key-space"
        ));
        assert!(!entry_covers("a*a", "a"));
    }

    #[test]
    fn a_case_that_a_pattern_covers_must_be_unsupported() {
        let listed_cases = listed(&["operations/s3/*"]);
        let mismatches = find_verdict_mismatches(
            &listed_cases,
            &graded(&[
                ("operations/s3/get", "unsupported"),
                ("operations/s3/head", "pass"),
                ("operations/azure/get", "pass"),
            ]),
        );
        let mismatched_ids: Vec<&str> = mismatches
            .iter()
            .map(|mismatch| mismatch["id"].as_str().unwrap())
            .collect();
        assert_eq!(mismatched_ids, ["operations/s3/head"]);
    }

    #[test]
    fn recording_keeps_patterns_and_lists_only_the_cases_they_miss() {
        let mut listed_cases = listed(&["operations/s3/*", "operations/s3/get"]);
        record_unsupported_cases(
            &mut listed_cases,
            &[
                json!({"id": "operations/s3/get", "verdict": "unsupported", "adapter_note": "no S3"}),
                json!({"id": "operations/azure/tier", "verdict": "unsupported", "adapter_note": "no tiers"}),
            ],
        );
        assert_eq!(
            listed_cases.keys().collect::<Vec<_>>(),
            ["operations/azure/tier", "operations/s3/*"]
        );
    }

    #[test]
    fn recording_replaces_graded_entries_and_keeps_the_rest() {
        let mut listed_cases = listed(&["now-supported", "not-graded"]);
        record_unsupported_cases(
            &mut listed_cases,
            &[
                json!({"id": "now-supported", "verdict": "pass"}),
                json!({"id": "new-limit", "verdict": "unsupported", "adapter_note": "no mapping"}),
                json!({"id": "field-limit", "verdict": "unsupported", "adapter_note": ["no MD5"]}),
            ],
        );
        assert_eq!(
            listed_cases.keys().collect::<Vec<_>>(),
            ["field-limit", "new-limit", "not-graded"]
        );
        assert_eq!(listed_cases["new-limit"], "no mapping");
        assert_eq!(listed_cases["field-limit"], "no MD5");
    }
}
