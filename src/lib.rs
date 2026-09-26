//! Matches client requests and results against JSON case assertions.
mod checks;
mod http_transport;
mod request;

pub mod expected_unsupported;
pub mod model;
pub mod runner;

pub use checks::{Difference, check_assertions};
pub use request::normalize_http_request;

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;

use checks::compile_full_match_regex;
use model::*;
use serde_json::{Value, json};
use std::{collections::VecDeque, time::SystemTime};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

/// Pending exchanges and grading failures for one adapter operation.
pub struct Session {
    id: String,
    lane: Lane,
    expect: Vec<Check>,
    refusal: Option<Vec<Check>>,
    decline_permitted: bool,
    profile: Profile,
    pending: VecDeque<Exchange>,
    completed: usize,
    pub(crate) failures: Vec<Value>,
}

impl Session {
    pub fn new(case: Case, profile: Profile) -> Self {
        Self {
            pending: case.exchanges.into(),
            id: case.id,
            lane: case.lane,
            expect: case.expect,
            refusal: case.refusal,
            decline_permitted: case.decline_permitted,
            profile,
            completed: 0,
            failures: vec![],
        }
    }

    pub fn respond(&mut self, request: &Value, now: SystemTime) -> Option<Response> {
        let Some(exchange) = self.pending.front() else {
            self.failures.push(json!({
                "exchange": self.completed,
                "error": "unexpected request",
            }));
            return None;
        };
        let mut differences = vec![];
        for alternative in &exchange.alternatives {
            let mut request_differences = check_assertions(&self.profile.checks, request, now);
            request_differences.extend(check_assertions(&alternative.request, request, now));
            for (field, allowed, extra) in [
                (
                    "headers",
                    &self.profile.allow_headers,
                    &alternative.allow_headers,
                ),
                ("query", &self.profile.allow_query, &alternative.allow_query),
            ] {
                let Some(fields) = request.get(field).and_then(Value::as_object) else {
                    continue;
                };
                for (name, value) in fields {
                    let pointer =
                        format!("/{field}/{}", name.replace('~', "~0").replace('/', "~1"));
                    let named = self
                        .profile
                        .checks
                        .iter()
                        .chain(&alternative.request)
                        .any(|check| check.at == pointer);
                    if !named
                        && !allowed.iter().chain(extra).any(|pattern| {
                            compile_full_match_regex(pattern)
                                .is_ok_and(|regex| regex.is_match(name))
                        })
                    {
                        request_differences.push(Difference {
                            at: pointer,
                            expected: Rule::Absent,
                            got: Some(value.clone()),
                        });
                    }
                }
            }
            if request_differences.is_empty() {
                let response = alternative.response.clone();
                let follow_up = alternative.then.clone();
                self.completed += 1;
                self.pending.pop_front();
                for exchange in follow_up.into_iter().rev() {
                    self.pending.push_front(exchange);
                }
                return Some(response);
            }
            differences.push(json!(request_differences));
        }
        self.failures.push(json!({
            "exchange": self.completed,
            "alternatives": differences,
        }));
        None
    }

    pub fn finish(&self, result: &Value, now: SystemTime) -> Value {
        let outcome = result.get("outcome").and_then(Value::as_str);
        let (checks, refused) = match &self.refusal {
            Some(checks) if outcome == Some("refused") && self.completed == 0 => (checks, true),
            _ => (&self.expect, false),
        };

        // An unsupported call passes where the service ignores one of its parameters, if
        // the adapter names that parameter, as a refusal must. Any other unsupported call
        // stays unsupported. A refusal of that call is graded by the refusal checks.
        let names_the_ignored_parameter = || {
            self.refusal.as_ref().is_some_and(|checks| {
                let parameter_checks: Vec<Check> = checks
                    .iter()
                    .filter(|check| check.at == "/parameter")
                    .cloned()
                    .collect();
                check_assertions(&parameter_checks, result, now).is_empty()
            })
        };
        let declined_before_sending = self.decline_permitted
            && self.completed == 0
            && outcome == Some("unsupported")
            && names_the_ignored_parameter();

        let differences = if declined_before_sending {
            vec![]
        } else {
            check_assertions(checks, result, now)
        };
        let required_pending = self
            .pending
            .iter()
            .filter(|exchange| !exchange.optional)
            .count();
        let missing = !refused && !declined_before_sending && required_pending > 0;

        // A missing field that the result declares in `unsupported_fields` makes the
        // case unsupported. A missing field without a declaration makes it wrong.
        let declared_unsupported_fields: Vec<&Value> = result
            .get("unsupported_fields")
            .and_then(Value::as_array)
            .map(|fields| fields.iter().collect())
            .unwrap_or_default();
        let declared_unsupported_field = |pointer: &str| {
            declared_unsupported_fields
                .iter()
                .copied()
                .find(|field| field.get("at").and_then(Value::as_str) == Some(pointer))
        };
        let only_declared_fields_missing = !differences.is_empty()
            && differences.iter().all(|difference| {
                difference.got.is_none() && declared_unsupported_field(&difference.at).is_some()
            });

        let verdict = if !self.failures.is_empty() {
            "wrong"
        } else if declined_before_sending {
            "pass"
        } else if outcome == Some("unsupported") && self.completed == 0 {
            "unsupported"
        } else if missing {
            "wrong"
        } else if only_declared_fields_missing {
            "unsupported"
        } else if !differences.is_empty() {
            "wrong"
        } else {
            "pass"
        };
        // The declared fields whose absence made the case unsupported, by pointer.
        let mut unsupported_fields = Vec::new();
        let (limitation_scope, adapter_note) = if outcome == Some("unsupported") {
            (
                result.get("scope").cloned().unwrap_or(json!("adapter")),
                result.get("reason").cloned(),
            )
        } else if verdict == "unsupported" {
            let limited_fields: Vec<&Value> = differences
                .iter()
                .filter_map(|difference| declared_unsupported_field(&difference.at))
                .collect();
            unsupported_fields = differences
                .iter()
                .map(|difference| difference.at.clone())
                .collect();
            let scope = limited_fields
                .iter()
                .find_map(|field| field.get("scope").cloned())
                .unwrap_or(json!("adapter"));
            let reasons: Vec<Value> = limited_fields
                .iter()
                .filter_map(|field| field.get("reason").cloned())
                .collect();
            (scope, Some(json!(reasons)))
        } else {
            (Value::Null, result.get("reason").cloned())
        };
        let mut report = json!({
            "id": self.id,
            "lane": self.lane,
            "verdict": verdict,
            "exchanges": self.completed,
            "required_exchanges": self.completed + required_pending,
            "request_failures": self.failures,
            "result_differences": differences,
            "adapter_note": adapter_note,
            "limitation_scope": limitation_scope,
        });
        if !unsupported_fields.is_empty() {
            report["unsupported_fields"] = json!(unsupported_fields);
        }
        report
    }
}
