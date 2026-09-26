use crate::{
    Result,
    checks::{compile_full_match_regex, normalize_xml},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub version: u32,
    pub profiles: BTreeMap<String, Profile>,
    pub cases: Vec<Case>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub provider: String,
    pub endpoint: Value,

    #[serde(default)]
    pub checks: Vec<Check>,

    /// Header-name patterns permitted in addition to the explicitly checked headers.
    #[serde(default)]
    pub allow_headers: Vec<String>,

    #[serde(default)]
    pub allow_query: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Core,
    Vectors,
    Live,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub profile: String,
    pub lane: Lane,
    pub purpose: String,
    pub sources: Vec<String>,

    /// The operation input passed unchanged to the adapter.
    pub call: Value,

    #[serde(default)]
    pub exchanges: Vec<Exchange>,
    pub expect: Vec<Check>,

    #[serde(default)]
    pub refusal: Option<Vec<Check>>,

    /// `true` if a client that reports the call unsupported before sending passes.
    ///
    /// The service ignores one parameter of such a call, such as a range it cannot
    /// serve. A refusal of the call is graded by `refusal`, which names that parameter.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub decline_permitted: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Exchange {
    /// Permitted requests and the response to send for each one.
    pub alternatives: Vec<Alternative>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Alternative {
    pub request: Vec<Check>,

    /// Required follow-up exchanges when this alternative is selected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub then: Vec<Exchange>,

    #[serde(default)]
    pub allow_headers: Vec<String>,

    #[serde(default)]
    pub allow_query: Vec<String>,
    pub response: Response,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub status: u16,

    #[serde(default)]
    pub headers: BTreeMap<String, Header>,

    #[serde(default)]
    pub body: Body,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Header {
    Literal(String),
    Dynamic(DynamicHeader),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "from", rename_all = "snake_case", deny_unknown_fields)]
pub enum DynamicHeader {
    Now,
    Request { name: String },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(
    tag = "encoding",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Body {
    #[default]
    Empty,
    Utf8(String),
    Base64(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub at: String,

    #[serde(default)]
    pub optional: bool,
    pub rule: Rule,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "is", rename_all = "snake_case", deny_unknown_fields)]
pub enum Rule {
    Equal {
        value: Value,
    },
    OneOf {
        values: Vec<Value>,
    },
    ArrayLength {
        value: usize,
    },
    Present,
    Absent,
    Matches {
        pattern: String,
    },
    Fresh {
        format: DateFormat,
        max_past_seconds: u64,
        max_future_seconds: u64,
    },
    Xml {
        value: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DateFormat {
    Http,
    Amz,
}

impl Body {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Empty => vec![],
            Self::Utf8(text) => text.as_bytes().to_vec(),
            Self::Base64(text) => STANDARD.decode(text)?,
        })
    }
}

fn is_valid_http_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn validate_checks(checks: &[Check]) -> Result<()> {
    for check in checks {
        if !check.at.is_empty() && !check.at.starts_with('/') {
            return Err(format!("invalid JSON pointer {}", check.at).into());
        }
        let mut pointer_characters = check.at.chars();
        while let Some(character) = pointer_characters.next() {
            if character == '~' && !matches!(pointer_characters.next(), Some('0' | '1')) {
                return Err(format!("invalid JSON pointer escape {}", check.at).into());
            }
        }
        if check.optional && matches!(check.rule, Rule::Absent) {
            return Err("optional absent check is redundant".into());
        }
        match &check.rule {
            Rule::Matches { pattern } => {
                compile_full_match_regex(pattern)?;
            }
            Rule::Xml { value } => {
                normalize_xml(value)?;
            }
            Rule::OneOf { values } if values.is_empty() => return Err("empty one_of".into()),
            _ => {}
        }
    }
    Ok(())
}

impl Suite {
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let suite: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        suite.validate()?;
        Ok(suite)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 || self.cases.is_empty() {
            return Err("expected version 1 and nonempty cases".into());
        }
        let mut case_ids = BTreeSet::new();
        for profile in self.profiles.values() {
            if profile.provider.is_empty() || !profile.endpoint.is_object() {
                return Err("profile requires a provider and an endpoint object".into());
            }
            validate_checks(&profile.checks)?;
            for pattern in profile.allow_headers.iter().chain(&profile.allow_query) {
                compile_full_match_regex(pattern)?;
            }
        }
        for case in &self.cases {
            if !case.call.is_object() {
                return Err(format!("{}: call must be an object", case.id).into());
            }
            if case.id.is_empty() || !case_ids.insert(&case.id) {
                return Err(format!("empty or duplicate id {}", case.id).into());
            }
            if !self.profiles.contains_key(&case.profile) {
                return Err(format!("unknown profile {}", case.profile).into());
            }
            if case.purpose.is_empty() || case.sources.is_empty() || case.expect.is_empty() {
                return Err(format!("{} lacks purpose, sources or expectations", case.id).into());
            }
            if matches!(case.lane, Lane::Core) && case.exchanges.is_empty() {
                return Err(format!("{}: core requires observed traffic", case.id).into());
            }
            if matches!(case.lane, Lane::Live) && !case.exchanges.is_empty() {
                return Err("live cases cannot replay responses".into());
            }
            validate_checks(&case.expect)?;
            if let Some(refusal) = &case.refusal {
                if refusal.is_empty() {
                    return Err("empty refusal".into());
                }
                validate_checks(refusal)?;
            }
            let mut exchanges: Vec<_> = case.exchanges.iter().collect();
            while let Some(exchange) = exchanges.pop() {
                if exchange.alternatives.is_empty() {
                    return Err("empty exchange".into());
                }
                for alternative in &exchange.alternatives {
                    exchanges.extend(&alternative.then);
                    validate_checks(&alternative.request)?;
                    for pattern in alternative
                        .allow_headers
                        .iter()
                        .chain(&alternative.allow_query)
                    {
                        compile_full_match_regex(pattern)?;
                    }
                    alternative.response.validate()?;
                }
            }
        }
        Ok(())
    }
}

impl Response {
    fn validate(&self) -> Result<()> {
        if !(200..=599).contains(&self.status) {
            return Err("invalid response status".into());
        }
        self.body.bytes()?;

        let mut names = BTreeSet::new();
        for (name, header) in &self.headers {
            if name.eq_ignore_ascii_case("transfer-encoding") {
                return Err("response framing is owned by the HTTP transport".into());
            }
            if !is_valid_http_header_name(name) {
                return Err("invalid response header name".into());
            }
            if !names.insert(name.to_ascii_lowercase()) {
                return Err("duplicate response header name".into());
            }

            if name.eq_ignore_ascii_case("content-length") {
                let Header::Literal(length) = header else {
                    return Err("response content-length must be a literal number".into());
                };
                length
                    .parse::<usize>()
                    .map_err(|_| "invalid response content-length")?;
            }

            match header {
                Header::Literal(value) => {
                    if !value
                        .bytes()
                        .all(|byte| byte == b'\t' || (32..=126).contains(&byte))
                    {
                        return Err("response header must contain printable ASCII or tabs".into());
                    }
                }
                Header::Dynamic(DynamicHeader::Request { name }) => {
                    if !is_valid_http_header_name(name) {
                        return Err("invalid echoed request header name".into());
                    }
                }
                Header::Dynamic(DynamicHeader::Now) => {}
            }
        }
        Ok(())
    }
}
