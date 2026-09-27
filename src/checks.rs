use crate::{
    Result,
    model::{Check, DateFormat, Rule},
};
use regex_lite::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime};

pub(crate) fn compile_full_match_regex(pattern: &str) -> Result<Regex> {
    Ok(Regex::new(&format!("\\A(?:{pattern})\\z"))?)
}

pub(crate) fn normalize_xml(value: &str) -> Result<Value> {
    fn normalize_xml_element(node: roxmltree::Node<'_, '_>) -> Value {
        let mut attributes: Vec<_> = node
            .attributes()
            .map(|attribute| {
                (
                    attribute.namespace().unwrap_or(""),
                    attribute.name(),
                    attribute.value(),
                )
            })
            .collect();
        attributes.sort();

        // Ignore indentation between elements, but retain whitespace in leaf values.
        let has_elements = node.children().any(|child| child.is_element());
        let mut children = Vec::new();
        for child in node.children() {
            match child.node_type() {
                roxmltree::NodeType::Element => children.push(normalize_xml_element(child)),
                roxmltree::NodeType::Text => {
                    let text = child.text().unwrap_or("");
                    if !has_elements || !text.trim().is_empty() {
                        children.push(json!(text));
                    }
                }
                _ => {}
            }
        }

        json!([
            node.tag_name().namespace().unwrap_or(""),
            node.tag_name().name(),
            attributes,
            children,
        ])
    }
    let document = roxmltree::Document::parse(value)?;
    Ok(normalize_xml_element(document.root_element()))
}

// Reuse httpdate's calendar validation, then enforce the AWS wire shape.
fn parse_amz_timestamp(timestamp: &str) -> Option<SystemTime> {
    if timestamp.len() != 16
        || !timestamp.is_ascii()
        || &timestamp[8..9] != "T"
        || &timestamp[15..] != "Z"
        || !timestamp[..8]
            .bytes()
            .chain(timestamp[9..15].bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = timestamp[4..6].parse::<usize>().ok()?.checked_sub(1)?;
    // httpdate validates the weekday; try the seven legal tokens without maintaining a calendar.
    ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        .iter()
        .find_map(|day| {
            httpdate::parse_http_date(&format!(
                "{day}, {} {} {} {}:{}:{} GMT",
                &timestamp[6..8],
                months.get(month)?,
                &timestamp[..4],
                &timestamp[9..11],
                &timestamp[11..13],
                &timestamp[13..15]
            ))
            .ok()
        })
}

fn rule_matches_value(rule: &Rule, got: Option<&Value>, now: SystemTime) -> bool {
    match rule {
        Rule::Equal { value } => got == Some(value),
        Rule::OneOf { values } => got.is_some_and(|value| values.contains(value)),
        Rule::ArrayLength { value } => got
            .and_then(Value::as_array)
            .is_some_and(|items| items.len() == *value),
        Rule::Present => got.is_some(),
        Rule::Absent => got.is_none(),
        Rule::Matches { pattern } => got.and_then(Value::as_str).is_some_and(|value| {
            compile_full_match_regex(pattern).is_ok_and(|regex| regex.is_match(value))
        }),
        Rule::Xml { value } => {
            let Some(got) = got.and_then(Value::as_str) else {
                return false;
            };
            match (normalize_xml(value), normalize_xml(got)) {
                (Ok(expected), Ok(actual)) => expected == actual,
                _ => false,
            }
        }
        Rule::Fresh {
            format,
            max_past_seconds,
            max_future_seconds,
        } => {
            let date = got.and_then(Value::as_str).and_then(|value| match format {
                DateFormat::Http => httpdate::parse_http_date(value).ok(),
                DateFormat::Amz => parse_amz_timestamp(value),
            });
            date.is_some_and(|date| match now.duration_since(date) {
                Ok(age) => age <= Duration::from_secs(*max_past_seconds),
                Err(future) => future.duration() <= Duration::from_secs(*max_future_seconds),
            })
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Difference {
    pub at: String,
    pub expected: Rule,
    pub got: Option<Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub because: Option<String>,
}

pub fn check_assertions(checks: &[Check], value: &Value, now: SystemTime) -> Vec<Difference> {
    let mut differences = Vec::new();
    for check in checks {
        let got = value.pointer(&check.at);
        if check.optional && got.is_none() {
            continue;
        }
        if !rule_matches_value(&check.rule, got, now) {
            differences.push(Difference {
                at: check.at.clone(),
                expected: check.rule.clone(),
                got: got.cloned(),
                because: check.because.clone(),
            });
        }
    }
    differences
}
