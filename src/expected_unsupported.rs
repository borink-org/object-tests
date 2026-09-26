//! A client's own list of the cases it expects to report unsupported.
//!
//! The list maps a suite name to entries, and each entry to the reason its cases
//! are unsupported. A suite name is the suite file's name without `.json`. An
//! entry is one of these:
//!
//! - A case ID.
//! - A pattern in which `*` matches any run of characters, such as
//!   `operations/s3/*` for every S3 case of the operations suite.
//! - A result field, such as `field:/value/content_md5_base64`, for a client
//!   that cannot report it. It covers every case that the client's result made
//!   unsupported only by leaving out fields the list names this way, as the
//!   report's `unsupported_fields` states.
//!
//! A run graded against the list passes only if every case an entry covers is
//! unsupported and every other case passes. A regression fails the run, and so
//! does a case that the client now supports. An entry that covers no case of
//! its suite is an error, and so is a field entry that covers no case of a run
//! that grades the whole suite. A case that stops passing because the client
//! stops reporting a listed field is covered, so the run does not notice it.
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

/// Returns whether a list entry can cover a case of the suite, before grading.
///
/// A field entry can cover a case whose expectations require that field: a check
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

// The fields whose absence made a graded case unsupported, by pointer.
fn unsupported_fields(report: &Value) -> Vec<&str> {
    report["unsupported_fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

// Whether field entries cover a graded case: it is unsupported only for fields
// that the list names.
fn fields_cover(listed_cases: &BTreeMap<String, String>, report: &Value) -> bool {
    let fields = unsupported_fields(report);
    report["verdict"] == "unsupported"
        && !fields.is_empty()
        && fields
            .iter()
            .all(|field| listed_cases.contains_key(&format!("{FIELD_ENTRY_PREFIX}{field}")))
}

// Whether a case ID or a pattern of the list matches a graded case.
fn id_listed(listed_cases: &BTreeMap<String, String>, case_id: &str) -> bool {
    listed_cases
        .keys()
        .any(|entry| !entry.starts_with(FIELD_ENTRY_PREFIX) && id_matches(entry, case_id))
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
/// case must pass. When the run graded the whole suite, a field entry that
/// covers no case is a mismatch too.
pub fn find_verdict_mismatches(
    listed_cases: &BTreeMap<String, String>,
    graded_reports: &[Value],
    graded_whole_suite: bool,
) -> Vec<Value> {
    let mut mismatches: Vec<Value> = graded_reports
        .iter()
        .filter_map(|report| {
            let case_id = report["id"].as_str()?;
            let verdict = report["verdict"].as_str()?;
            let covered = id_listed(listed_cases, case_id) || fields_cover(listed_cases, report);
            let expected_verdict = if covered { "unsupported" } else { "pass" };
            (verdict != expected_verdict).then(|| {
                json!({
                    "id": case_id,
                    "expected": expected_verdict,
                    "verdict": verdict,
                })
            })
        })
        .collect();
    if graded_whole_suite {
        for entry in listed_cases.keys() {
            let Some(field) = entry.strip_prefix(FIELD_ENTRY_PREFIX) else {
                continue;
            };
            let covers_a_case = graded_reports.iter().any(|report| {
                fields_cover(listed_cases, report) && unsupported_fields(report).contains(&field)
            });
            if !covers_a_case {
                mismatches.push(json!({"entry": entry, "expected": "a case it covers"}));
            }
        }
    }
    mismatches
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
    graded_reports: &[Value],
) {
    for report in graded_reports {
        let Some(case_id) = report["id"].as_str() else {
            continue;
        };
        let covered_by_category = fields_cover(listed_cases, report)
            || listed_cases
                .keys()
                .any(|entry| entry.contains('*') && id_matches(entry, case_id));
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

    fn report(case_id: &str, verdict: &str) -> Value {
        json!({"id": case_id, "verdict": verdict, "adapter_note": "reason"})
    }

    // A case left unsupported because the result lacked these declared fields.
    fn lacking(case_id: &str, fields: &[&str]) -> Value {
        json!({
            "id": case_id,
            "verdict": "unsupported",
            "adapter_note": ["no such header"],
            "unsupported_fields": fields,
        })
    }

    fn mismatched(mismatches: &[Value]) -> Vec<&str> {
        mismatches
            .iter()
            .map(|mismatch| {
                mismatch["id"]
                    .as_str()
                    .or(mismatch["entry"].as_str())
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn listed_unsupported_and_unlisted_passes_match() {
        let mismatches = find_verdict_mismatches(
            &listed(&["snapshot"]),
            &[report("snapshot", "unsupported"), report("get", "pass")],
            true,
        );
        assert!(mismatches.is_empty());
    }

    #[test]
    fn regressions_new_support_and_wrong_answers_mismatch() {
        let mismatches = find_verdict_mismatches(
            &listed(&["snapshot", "version"]),
            &[
                report("get", "unsupported"),
                report("snapshot", "pass"),
                report("version", "wrong"),
                report("head", "failed"),
            ],
            true,
        );
        assert_eq!(
            mismatched(&mismatches),
            ["get", "snapshot", "version", "head"]
        );
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
            &[
                report("operations/s3/get", "unsupported"),
                report("operations/s3/head", "pass"),
                report("operations/azure/get", "pass"),
            ],
            true,
        );
        assert_eq!(mismatched(&mismatches), ["operations/s3/head"]);
    }

    #[test]
    fn a_field_entry_covers_the_cases_that_lack_only_listed_fields() {
        let md5 = "/value/content_md5_base64";
        let language = "/value/content_language";
        let entries = listed(&["field:/value/content_md5_base64"]);
        let mismatches = find_verdict_mismatches(
            &entries,
            &[
                lacking("get", &[md5]),
                // A case that passes another way, such as a refusal, stays a pass.
                report("get-range", "pass"),
                // A case that also lacks an unlisted field is not covered.
                lacking("get-properties", &[md5, language]),
                // Nor is a case the adapter reports unsupported for another reason.
                report("get-snapshot", "unsupported"),
            ],
            true,
        );
        assert_eq!(mismatched(&mismatches), ["get-properties", "get-snapshot"]);
    }

    #[test]
    fn a_field_entry_that_covers_nothing_is_stale_on_a_whole_suite() {
        let entries = listed(&["field:/value/content_md5_base64"]);
        let graded = [report("get", "pass")];
        assert_eq!(
            mismatched(&find_verdict_mismatches(&entries, &graded, true)),
            ["field:/value/content_md5_base64"]
        );
        assert!(find_verdict_mismatches(&entries, &graded, false).is_empty());
    }

    #[test]
    fn a_field_entry_can_only_cover_cases_that_expect_the_field() {
        let case: Case = serde_json::from_value(json!({
            "id": "get",
            "profile": "azure",
            "lane": "core",
            "purpose": "test",
            "sources": ["test"],
            "call": {},
            "expect": [
                {"at": "/value/content_md5_base64", "rule": {"is": "present"}},
                {"at": "/value/cache_control", "optional": true, "rule": {"is": "present"}},
                {"at": "/value/content_language", "rule": {"is": "absent"}},
            ],
        }))
        .unwrap();
        assert!(entry_covers("field:/value/content_md5_base64", &case));
        assert!(!entry_covers("field:/value/cache_control", &case));
        assert!(!entry_covers("field:/value/content_language", &case));
        assert!(!entry_covers("field:/value/size", &case));
    }

    #[test]
    fn recording_keeps_categories_and_lists_only_the_cases_they_miss() {
        let mut listed_cases = listed(&[
            "operations/s3/*",
            "operations/s3/get",
            "field:/value/content_md5_base64",
        ]);
        record_unsupported_cases(
            &mut listed_cases,
            &[
                report("operations/s3/get", "unsupported"),
                report("operations/azure/tier", "unsupported"),
                lacking("operations/azure/get", &["/value/content_md5_base64"]),
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
