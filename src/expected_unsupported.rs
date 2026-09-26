//! A client's own list of the cases it expects to report unsupported.
//!
//! The list maps a suite name to entries, and each entry to the reason its cases
//! are unsupported. A suite name is the suite file's name without `.json`. An
//! entry is one of these:
//!
//! - A case ID.
//! - A pattern in which `*` matches any run of characters, such as
//!   `operations/s3/*` for every S3 case of the operations suite.
//! - A result field, such as `field:/value/content_md5_base64`. It covers every
//!   case whose expectations require that field, for a client that cannot
//!   report it.
//!
//! A run graded against the list passes only if every case an entry covers is
//! unsupported and every other case passes. A regression fails the run, and so
//! does a case that the client now supports. An entry that covers no case of
//! its suite is an error.
use crate::{
    Result,
    model::{Case, Rule, Suite},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

/// Entries and the reason their cases are unsupported, by suite name.
pub type ExpectedUnsupportedCases = BTreeMap<String, BTreeMap<String, String>>;

/// The prefix of an entry that names a result field rather than cases.
pub const FIELD_ENTRY_PREFIX: &str = "field:";

/// Returns whether a list entry covers a case.
///
/// A field entry covers a case whose expectations require that field: a check
/// at its pointer that is not optional and does not require it absent. Any
/// other entry covers the case whose ID it matches.
pub fn entry_covers(entry: &str, case: &Case) -> bool {
    match entry.strip_prefix(FIELD_ENTRY_PREFIX) {
        Some(pointer) => case.expect.iter().any(|check| {
            check.at == pointer && !check.optional && !matches!(check.rule, Rule::Absent)
        }),
        None => id_matches(entry, &case.id),
    }
}

/// Returns whether an entry matches a case ID: the entry is the ID, or a pattern
/// whose `*` wildcards match any run of characters in it.
pub fn id_matches(entry: &str, case_id: &str) -> bool {
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

// An entry written by hand for a whole category of cases, which recording keeps.
fn names_a_category(entry: &str) -> bool {
    entry.contains('*') || entry.starts_with(FIELD_ENTRY_PREFIX)
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
        .filter(|entry| !suite.cases.iter().any(|case| entry_covers(entry, case)))
        .map(String::as_str)
        .collect()
}

/// Returns a mismatch for every graded case whose verdict differs from the
/// expected one. A case that an entry covers must be `unsupported`, and any other
/// case must pass. `cases` holds the suite's cases, to read what they expect.
pub fn find_verdict_mismatches(
    listed_cases: &BTreeMap<String, String>,
    cases: &[Case],
    graded_verdicts: &[(String, String)],
) -> Vec<Value> {
    let covered = |case_id: &str| match cases.iter().find(|case| case.id == case_id) {
        Some(case) => listed_cases.keys().any(|entry| entry_covers(entry, case)),
        None => listed_cases.keys().any(|entry| id_matches(entry, case_id)),
    };
    graded_verdicts
        .iter()
        .filter_map(|(case_id, verdict)| {
            let expected_verdict = if covered(case_id) {
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
/// keeping entries for cases this run did not grade. Patterns and field entries
/// are written by hand and kept: a case that one covers gets no entry of its own.
pub fn record_unsupported_cases(
    listed_cases: &mut BTreeMap<String, String>,
    cases: &[Case],
    graded_reports: &[Value],
) {
    for report in graded_reports {
        let Some(case_id) = report["id"].as_str() else {
            continue;
        };
        let covered_by_category =
            cases
                .iter()
                .find(|case| case.id == case_id)
                .is_some_and(|case| {
                    listed_cases
                        .keys()
                        .any(|entry| names_a_category(entry) && entry_covers(entry, case))
                });
        if report["verdict"] == "unsupported" && !covered_by_category {
            listed_cases.insert(case_id.to_owned(), unsupported_reason(report));
        } else {
            listed_cases.remove(case_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(entries: &[&str]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|entry| (entry.to_string(), "reason".to_owned()))
            .collect()
    }

    fn graded(verdicts: &[(&str, &str)]) -> Vec<(String, String)> {
        verdicts
            .iter()
            .map(|(case_id, verdict)| (case_id.to_string(), verdict.to_string()))
            .collect()
    }

    // A case whose expectations require the given result fields.
    fn case_expecting(case_id: &str, fields: &[&str]) -> Case {
        serde_json::from_value(json!({
            "id": case_id,
            "profile": "azure",
            "lane": "core",
            "purpose": "test",
            "sources": ["test"],
            "call": {},
            "expect": fields
                .iter()
                .map(|field| json!({"at": field, "rule": {"is": "present"}}))
                .collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    fn cases(case_ids: &[&str]) -> Vec<Case> {
        case_ids
            .iter()
            .map(|case_id| case_expecting(case_id, &[]))
            .collect()
    }

    #[test]
    fn listed_unsupported_and_unlisted_passes_match() {
        let mismatches = find_verdict_mismatches(
            &listed(&["snapshot"]),
            &cases(&["snapshot", "get"]),
            &graded(&[("snapshot", "unsupported"), ("get", "pass")]),
        );
        assert!(mismatches.is_empty());
    }

    #[test]
    fn regressions_new_support_and_wrong_answers_mismatch() {
        let mismatches = find_verdict_mismatches(
            &listed(&["snapshot", "version"]),
            &cases(&["get", "snapshot", "version", "head"]),
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
        assert!(id_matches("operations/s3/get", "operations/s3/get"));
        assert!(!id_matches("operations/s3/get", "operations/s3/get-whole"));
        assert!(id_matches("operations/s3/*", "operations/s3/get-whole"));
        assert!(!id_matches(
            "operations/s3/*",
            "operations/s3-express/get-whole"
        ));
        assert!(id_matches(
            "operations/azure*/lease-*",
            "operations/azure-hns/lease-break"
        ));
        assert!(!id_matches(
            "operations/azure*/lease-*",
            "operations/azure/put-leased"
        ));
        assert!(id_matches("*-key-*", "operations/s3/put-created-key-space"));
        assert!(!id_matches("a*a", "a"));
    }

    #[test]
    fn a_case_that_a_pattern_covers_must_be_unsupported() {
        let mismatches = find_verdict_mismatches(
            &listed(&["operations/s3/*"]),
            &cases(&[
                "operations/s3/get",
                "operations/s3/head",
                "operations/azure/get",
            ]),
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
    fn a_field_entry_covers_the_cases_that_require_the_field() {
        let entry = "field:/value/content_md5_base64";
        let requires = case_expecting("get", &["/value/content_md5_base64"]);
        assert!(entry_covers(entry, &requires));
        assert!(!entry_covers(
            entry,
            &case_expecting("head", &["/value/size"])
        ));

        let mut optional = requires.clone();
        optional.expect[0].optional = true;
        assert!(!entry_covers(entry, &optional));
        let mut absent = requires.clone();
        absent.expect[0].rule = Rule::Absent;
        assert!(!entry_covers(entry, &absent));

        let mismatches = find_verdict_mismatches(
            &listed(&[entry]),
            &[requires, case_expecting("head", &["/value/size"])],
            &graded(&[("get", "unsupported"), ("head", "unsupported")]),
        );
        let mismatched_ids: Vec<&str> = mismatches
            .iter()
            .map(|mismatch| mismatch["id"].as_str().unwrap())
            .collect();
        assert_eq!(mismatched_ids, ["head"]);
    }

    #[test]
    fn recording_keeps_categories_and_lists_only_the_cases_they_miss() {
        let mut listed_cases = listed(&[
            "operations/s3/*",
            "operations/s3/get",
            "field:/value/content_md5_base64",
        ]);
        let suite_cases = [
            case_expecting("operations/s3/get", &[]),
            case_expecting("operations/azure/tier", &[]),
            case_expecting("operations/azure/get", &["/value/content_md5_base64"]),
        ];
        record_unsupported_cases(
            &mut listed_cases,
            &suite_cases,
            &[
                json!({"id": "operations/s3/get", "verdict": "unsupported", "adapter_note": "no S3"}),
                json!({"id": "operations/azure/tier", "verdict": "unsupported", "adapter_note": "no tiers"}),
                json!({"id": "operations/azure/get", "verdict": "unsupported", "adapter_note": "no MD5"}),
            ],
        );
        assert_eq!(
            listed_cases.keys().collect::<Vec<_>>(),
            [
                "field:/value/content_md5_base64",
                "operations/azure/tier",
                "operations/s3/*"
            ]
        );
    }

    #[test]
    fn recording_replaces_graded_entries_and_keeps_the_rest() {
        let mut listed_cases = listed(&["now-supported", "not-graded"]);
        record_unsupported_cases(
            &mut listed_cases,
            &cases(&["now-supported", "not-graded", "new-limit", "field-limit"]),
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
