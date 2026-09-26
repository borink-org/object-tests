//! Adapter for `borink-object-storage-proto`, the sans-IO Azure Blob crate of
//! borink-org/object-storage. The crate encodes each request into buffers that
//! this adapter sizes, and reads the response head and body that it hands back.
//! `ureq` sends the requests, as in the crate's own ureq host.
//!
//! The crate authenticates with a bearer token only. Offline cases receive a
//! placeholder token, and live cases need `endpoint.auth = "bearer"`.

use base64::{Engine, engine::general_purpose::STANDARD};
use borink_crypto::{Checksum, Crc64, Md5};
use borink_object_storage_proto::azure::{
    self, Block, BlockListKind, BlockRef, BlockSource, BlockState, PhysicalCommitBlocks,
    PhysicalListBlocks, PhysicalStageBlock,
};
use borink_object_storage_proto::{
    AzureNamespace, Blobs, CommitBlocksHeadOutcome, ConditionKind, Container, DeleteHeadOutcome,
    DeleteKind, EntryKind, Error as CrateError, GetHeadOutcome, GetKind, HeaderSpan,
    ListBlocksHeadOutcome, ListEntry, ListHeadOutcome, ListInclude, MetadataPair, Payload,
    PhysicalDelete, PhysicalGet, PhysicalList, PhysicalPut, PutHeadOutcome, RequestSize,
    RequestedRange, ResponseHead, StageBlockHeadOutcome, Timestamps, TransactionalChecksum,
    WireRequest, WriteOptions, layered,
};
use serde_json::{Map, Value, json};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

type AdapterError = Box<dyn std::error::Error>;

// Azure returns at most 5,000 entries on one listing page.
const AZURE_MAX_PAGE_ENTRIES: usize = 5_000;

const MAX_RESPONSE_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// One HTTP exchange, held whole so the crate can borrow its head and body.
struct HttpExchange {
    status: u16,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

impl HttpExchange {
    fn response_head(&self) -> ResponseHead<'_> {
        ResponseHead::from_headers(
            self.status,
            self.headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_slice())),
        )
    }

    fn error_result(&self, status: u16) -> Value {
        let head = self.response_head();
        let code = azure::error_code(&head, &self.body)
            .map(|code| String::from_utf8_lossy(code).into_owned());

        let mut result = json!({
            "outcome": "error",
            "status": status,
            "kind": error_kind_for_status(status),
        });
        if let Some(code) = code {
            result["code"] = json!(code);
        }
        result
    }
}

struct AdapterContext {
    http_agent: ureq::Agent,
}

fn error_kind_for_status(status: u16) -> &'static str {
    match status {
        404 => "not_found",
        412 => "precondition",
        304 => "not_modified",
        401 | 403 => "permission_denied",
        _ => "other",
    }
}

fn successful_result(value: Value) -> Value {
    json!({"outcome": "ok", "value": value})
}

fn unsupported_by_crate(reason: &str) -> Value {
    json!({"outcome": "unsupported", "scope": "sdk", "reason": reason})
}

fn unsupported_by_adapter(reason: &str) -> Value {
    json!({"outcome": "unsupported", "scope": "adapter", "reason": reason})
}

/// Writes `UnsupportedRange` as `unsupported_range`.
fn snake_case_name(debug_name: &str) -> String {
    let mut snake_case = String::new();
    for character in debug_name.chars() {
        if character.is_ascii_uppercase() {
            if !snake_case.is_empty() {
                snake_case.push('_');
            }
            snake_case.push(character.to_ascii_lowercase());
        } else {
            snake_case.push(character);
        }
    }
    snake_case
}

// The account kind this process was started for, which a refusal reads.
static ACCOUNT_NAMESPACE: std::sync::OnceLock<AzureNamespace> = std::sync::OnceLock::new();

// The call this process was started for, whose fields a refusal names.
static CURRENT_CALL: std::sync::OnceLock<Value> = std::sync::OnceLock::new();

/// The call field that the crate's reason for refusing a plan is about.
fn refused_call_parameter(reason_name: &str, call: &Value) -> Option<&'static str> {
    let named_field = |field: &'static str, alternative: &'static str| {
        if call.get(field).is_some() {
            field
        } else {
            alternative
        }
    };
    Some(match reason_name {
        "EmptyKey" | "UrlTooLong" | "RequestTooLarge" => "key",
        reason if reason.starts_with("Key") => "key",
        "Range" | "UnsupportedRange" | "RangedHead" => "range",
        "Condition" => named_field("if_match", "if_none_match"),
        "PayloadTooLarge" => "body_base64",
        "BlockId" => named_field("block_id_base64", "blocks"),
        "Blocks" => "blocks",
        "Prefix" => "prefix",
        "Marker" => "continuation_token",
        "MaxResults" => "page_size",
        reason if reason.starts_with("Metadata") => "metadata",
        "Checksum" => "checksum",
        _ => return None,
    })
}

/// Returns the result for an error from the crate.
///
/// A plan that the crate will not encode becomes a refusal to send. If the crate
/// names the answer the account would give, the refusal carries it. Any other
/// error becomes a failed operation.
fn result_for_crate_error(error: CrateError) -> Value {
    match error {
        CrateError::InvalidPlan(invalid_plan) => {
            let reason_name = format!("{invalid_plan:?}");
            let mut result = json!({
                "outcome": "refused",
                "kind": snake_case_name(&reason_name),
            });
            let call = CURRENT_CALL.get().unwrap_or(&Value::Null);
            if let Some(parameter) = refused_call_parameter(&reason_name, call) {
                result["parameter"] = json!(parameter);
            }
            let namespace = ACCOUNT_NAMESPACE
                .get()
                .copied()
                .unwrap_or(AzureNamespace::Unknown);
            if let Some(rejection) = invalid_plan.azure_rejection(namespace) {
                result["status"] = json!(rejection.status);
                result["code"] = json!(rejection.code);
            }
            result
        }
        other => json!({
            "outcome": "error",
            "kind": "other",
            "reason": format!("{other:?}"),
        }),
    }
}

macro_rules! crate_step {
    ($expression:expr) => {
        match $expression {
            Ok(value) => value,
            Err(error) => return Ok(result_for_crate_error(error)),
        }
    };
}

macro_rules! transport_step {
    ($expression:expr) => {
        match $expression {
            Ok(value) => value,
            Err(error) => {
                return Ok(json!({
                    "outcome": "error",
                    "kind": "transport",
                    "reason": error.to_string(),
                }));
            }
        }
    };
}

fn current_timestamps() -> Timestamps {
    let seconds_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    Timestamps::from_unix(seconds_since_epoch)
}

// The crate sizes a request as bytes and header slots, and writes into both.
fn request_buffers(size: RequestSize) -> (Vec<u8>, Vec<HeaderSpan>) {
    (
        vec![0; size.bytes],
        vec![HeaderSpan::default(); size.headers],
    )
}

fn send_request(
    context: &AdapterContext,
    request: &WireRequest<'_>,
) -> Result<HttpExchange, ureq::Error> {
    let mut builder = ureq::http::Request::builder()
        .method(request.method().as_str())
        .uri(request.url());
    for (name, value) in request.headers() {
        builder = builder.header(name, value);
    }

    let mut response = match request.payload().bytes() {
        Some(payload) => context.http_agent.run(builder.body(payload.to_vec())?)?,
        None => context.http_agent.run(builder.body(())?)?,
    };

    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
        .collect();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BODY_BYTES)
        .read_to_vec()?;
    Ok(HttpExchange {
        status,
        headers,
        body,
    })
}

fn decode_base64_field(call: &Value, field: &str) -> Result<Vec<u8>, AdapterError> {
    let text = call
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {field}"))?;
    Ok(STANDARD.decode(text)?)
}

fn optional_text<'a>(call: &'a Value, field: &str) -> Option<&'a str> {
    call.get(field).and_then(Value::as_str)
}

fn text_of(bytes: Option<&[u8]>) -> Option<String> {
    bytes.map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

/// The crate takes one precondition per request.
fn requested_condition(call: &Value) -> Option<(ConditionKind, Option<&[u8]>)> {
    match (
        optional_text(call, "if_match"),
        optional_text(call, "if_none_match"),
    ) {
        (None, None) => Some((ConditionKind::None, None)),
        (Some(tag), None) => Some((ConditionKind::IfMatch, Some(tag.as_bytes()))),
        (None, Some(tag)) => Some((ConditionKind::IfNoneMatch, Some(tag.as_bytes()))),
        (Some(_), Some(_)) => None,
    }
}

fn requested_range(call: &Value) -> Result<RequestedRange, AdapterError> {
    let Some(range) = call.get("range") else {
        return Ok(RequestedRange::Whole);
    };

    if let Some(suffix_length) = range.get("suffix").and_then(Value::as_u64) {
        return Ok(RequestedRange::Suffix(suffix_length));
    }

    let start = range
        .get("start")
        .and_then(Value::as_u64)
        .ok_or("range without start")?;
    Ok(match range.get("end").and_then(Value::as_u64) {
        Some(end) => RequestedRange::Bounded { start, end },
        None => RequestedRange::Offset(start),
    })
}

/// Returns `true` if the call selects a snapshot or a version, which the crate's plans cannot.
fn names_snapshot_or_version(call: &Value) -> bool {
    call.get("snapshot").is_some() || call.get("version").is_some()
}

fn metadata_from_headers(exchange: &HttpExchange) -> Map<String, Value> {
    exchange
        .headers
        .iter()
        .filter_map(|(name, value)| {
            azure::metadata_name(name)
                .map(|name| (name.to_owned(), json!(String::from_utf8_lossy(value))))
        })
        .collect()
}

fn read_object(
    context: &AdapterContext,
    blobs: &Blobs<'_>,
    call: &Value,
    kind: GetKind,
) -> Result<Value, AdapterError> {
    if names_snapshot_or_version(call) {
        return Ok(unsupported_by_crate(
            "PhysicalGet selects no snapshot or version",
        ));
    }
    let Some((condition, condition_value)) = requested_condition(call) else {
        return Ok(unsupported_by_crate("PhysicalGet carries one precondition"));
    };

    let key = optional_text(call, "key").unwrap_or_default();
    let range = if kind == GetKind::Head {
        RequestedRange::Whole
    } else {
        requested_range(call)?
    };
    let get_plan = PhysicalGet {
        key,
        kind,
        range,
        condition,
        condition_value,
    };

    let now = current_timestamps();
    let (mut request_bytes, mut header_spans) = request_buffers(crate_step!(
        layered::get_requirements(blobs, &get_plan, &now)
    ));
    let request =
        crate_step!(blobs.encode_get(&mut request_bytes, &mut header_spans, &get_plan, &now));
    let exchange = transport_step!(send_request(context, &request));

    let head_outcome =
        crate_step!(blobs.accept_get_head(get_plan.shape(), exchange.response_head()));
    let outcome = match head_outcome {
        GetHeadOutcome::NeedErrorBody(failure) => {
            blobs.accept_error_body(failure.status, failure.request_id, &exchange.body)
        }
        outcome => outcome,
    };

    Ok(match outcome {
        GetHeadOutcome::Body { meta, .. } | GetHeadOutcome::Complete { meta } => {
            // The crate does not read Content-MD5 from a response head.
            let unsupported_fields = json!([{
                "at": "/value/content_md5_base64",
                "scope": "sdk",
                "reason": "ResponseHead and ObjectMeta carry no Content-MD5",
            }]);
            let mut value = json!({
                "etag": text_of(meta.e_tag).unwrap_or_default(),
                "metadata": metadata_from_headers(&exchange),
            });
            if let Some(content_type) = text_of(meta.content_type) {
                value["content_type"] = json!(content_type);
            }
            if let Some(version) = text_of(meta.version) {
                value["version"] = json!(version);
            }

            if kind == GetKind::Head {
                value["size"] = json!(meta.size.unwrap_or(0));
            } else {
                value["body_base64"] = json!(STANDARD.encode(&exchange.body));
                value["size"] = json!(exchange.body.len());
            }
            let mut result = successful_result(value);
            result["unsupported_fields"] = unsupported_fields;
            result
        }
        GetHeadOutcome::NotModified { .. } => exchange.error_result(304),
        GetHeadOutcome::PreconditionFailed => exchange.error_result(412),
        GetHeadOutcome::NotFound { .. } => exchange.error_result(404),
        GetHeadOutcome::RangeNotSatisfiable { .. } => exchange.error_result(416),
        GetHeadOutcome::NeedErrorBody(failure) | GetHeadOutcome::ServiceFailure(failure) => {
            exchange.error_result(failure.status)
        }
        _ => exchange.error_result(exchange.status),
    })
}

fn write_object(
    context: &AdapterContext,
    blobs: &Blobs<'_>,
    call: &Value,
) -> Result<Value, AdapterError> {
    if call.get("content_type").is_some() {
        return Ok(unsupported_by_crate("PhysicalPut sets no content type"));
    }
    // A plan holds one transactional checksum, so the crate cannot send a write
    // that names two. Azure refuses such a write too.
    if call.get("checksums").is_some() {
        return Ok(json!({
            "outcome": "refused",
            "kind": "one_transactional_checksum",
            "parameter": "checksums",
        }));
    }
    let Some((condition, condition_value)) = requested_condition(call) else {
        return Ok(unsupported_by_crate("PhysicalPut carries one precondition"));
    };

    let checksum_algorithm = call.pointer("/checksum/algorithm").and_then(Value::as_str);
    let checksum_value = call
        .pointer("/checksum/value_base64")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let checksum = match checksum_algorithm {
        None => None,
        Some("md5") => Some(TransactionalChecksum::Md5(checksum_value)),
        Some("azure_crc64") => Some(TransactionalChecksum::Crc64(checksum_value)),
        Some(_) => return Ok(unsupported_by_crate("checksum algorithm")),
    };

    let metadata_pairs: Vec<MetadataPair<'_>> = call
        .get("metadata")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, value)| MetadataPair {
            name,
            value: value.as_str().unwrap_or_default(),
        })
        .collect();

    let key = optional_text(call, "key").unwrap_or_default();
    let body = decode_base64_field(call, "body_base64")?;
    let put_plan = PhysicalPut {
        key,
        condition,
        condition_value,
        metadata: &metadata_pairs,
        options: WriteOptions {
            checksum,
            ..WriteOptions::default()
        },
    };
    let payload = Payload::Slice(&body);

    let now = current_timestamps();
    let (mut request_bytes, mut header_spans) = request_buffers(crate_step!(
        layered::put_requirements(blobs, &put_plan, payload, &now)
    ));
    let request = crate_step!(blobs.encode_put(
        &mut request_bytes,
        &mut header_spans,
        &put_plan,
        payload,
        &now
    ));
    let exchange = transport_step!(send_request(context, &request));

    let head_outcome =
        crate_step!(blobs.accept_put_head(put_plan.shape(), exchange.response_head()));
    let outcome = match head_outcome {
        PutHeadOutcome::NeedErrorBody(failure) => {
            blobs.accept_put_error_body(failure.status, failure.request_id, &exchange.body)
        }
        outcome => outcome,
    };

    Ok(match outcome {
        PutHeadOutcome::Created { meta, .. } => {
            let mut value = json!({"etag": text_of(meta.e_tag).unwrap_or_default()});
            if let Some(version) = text_of(meta.version) {
                value["version"] = json!(version);
            }
            successful_result(value)
        }
        PutHeadOutcome::PreconditionFailed => exchange.error_result(exchange.status),
        PutHeadOutcome::NotFound { .. } => exchange.error_result(404),
        PutHeadOutcome::NeedErrorBody(failure) | PutHeadOutcome::ServiceFailure(failure) => {
            exchange.error_result(failure.status)
        }
        _ => exchange.error_result(exchange.status),
    })
}

fn delete_object(
    context: &AdapterContext,
    blobs: &Blobs<'_>,
    call: &Value,
) -> Result<Value, AdapterError> {
    if names_snapshot_or_version(call) {
        return Ok(unsupported_by_crate(
            "PhysicalDelete selects no snapshot or version",
        ));
    }
    let Some((condition, condition_value)) = requested_condition(call) else {
        return Ok(unsupported_by_crate(
            "PhysicalDelete carries one precondition",
        ));
    };
    let delete_kind = match optional_text(call, "snapshots") {
        None => DeleteKind::Object,
        Some("only") => DeleteKind::SnapshotsOnly,
        Some("include") => DeleteKind::ObjectAndSnapshots,
        Some(_) => return Err("unknown snapshots mode".into()),
    };

    let key = optional_text(call, "key").unwrap_or_default();
    let delete_plan = PhysicalDelete {
        key,
        kind: delete_kind,
        condition,
        condition_value,
    };

    let now = current_timestamps();
    let (mut request_bytes, mut header_spans) = request_buffers(crate_step!(
        layered::delete_requirements(blobs, &delete_plan, &now)
    ));
    let request =
        crate_step!(blobs.encode_delete(&mut request_bytes, &mut header_spans, &delete_plan, &now));
    let exchange = transport_step!(send_request(context, &request));

    let head_outcome =
        crate_step!(blobs.accept_delete_head(delete_plan.shape(), exchange.response_head()));
    let outcome = match head_outcome {
        DeleteHeadOutcome::NeedErrorBody(failure) => {
            blobs.accept_delete_error_body(failure.status, failure.request_id, &exchange.body)
        }
        outcome => outcome,
    };

    Ok(match outcome {
        DeleteHeadOutcome::Accepted => successful_result(json!({})),
        DeleteHeadOutcome::PreconditionFailed => exchange.error_result(exchange.status),
        DeleteHeadOutcome::NotFound { .. } => exchange.error_result(404),
        DeleteHeadOutcome::NeedErrorBody(failure) | DeleteHeadOutcome::ServiceFailure(failure) => {
            exchange.error_result(failure.status)
        }
        _ => exchange.error_result(exchange.status),
    })
}

/// The decoded text of a listed value, such as a metadata name or value.
fn decoded_listing_text(raw_value: &[u8]) -> String {
    let mut decoded = vec![0; raw_value.len()];
    match layered::decode_into(raw_value, &mut decoded) {
        Some(text) => String::from_utf8_lossy(text).into_owned(),
        None => String::from_utf8_lossy(raw_value).into_owned(),
    }
}

fn listed_entry_value(entry: &ListEntry<'_>, metadata_requested: bool) -> Value {
    let mut value = json!({
        "key": entry.key,
        "size": entry.size.unwrap_or(0),
        "etag": entry.e_tag.unwrap_or_default(),
    });

    if let Some(metadata) = entry.metadata() {
        let pairs: Map<String, Value> = metadata
            .map(|(name, value)| {
                (
                    decoded_listing_text(name),
                    json!(decoded_listing_text(value)),
                )
            })
            .collect();
        value["metadata"] = Value::Object(pairs);
    } else if metadata_requested {
        value["metadata"] = json!({});
    }

    if entry.kind == EntryKind::Directory {
        value["directory"] = json!(true);
    }

    let listed_properties = [
        ("snapshot", "Snapshot"),
        ("version", "VersionId"),
        ("content_md5_base64", "Content-MD5"),
        ("content_crc64_base64", "Content-CRC64"),
    ];
    for (field, property_name) in listed_properties {
        if let Some(property) = entry
            .property(property_name)
            .filter(|bytes| !bytes.is_empty())
        {
            value[field] = json!(decoded_listing_text(property));
        }
    }
    if let Some(is_current_version) = entry.property("IsCurrentVersion") {
        value["is_current_version"] = json!(is_current_version == b"true");
    }
    value
}

struct ListedPage {
    entries: Vec<Value>,
    prefixes: Vec<String>,
    next_marker: Option<String>,
}

/// Requests one page and reads it, or returns the result to report instead.
fn list_one_page(
    context: &AdapterContext,
    blobs: &Blobs<'_>,
    list_plan: &PhysicalList<'_>,
) -> Result<Result<ListedPage, Value>, AdapterError> {
    macro_rules! crate_step_in_page {
        ($expression:expr) => {
            match $expression {
                Ok(value) => value,
                Err(error) => return Ok(Err(result_for_crate_error(error))),
            }
        };
    }

    let now = current_timestamps();
    let (mut request_bytes, mut header_spans) = request_buffers(crate_step_in_page!(
        layered::list_requirements(blobs, list_plan, &now)
    ));
    let request = crate_step_in_page!(blobs.encode_list(
        &mut request_bytes,
        &mut header_spans,
        list_plan,
        &now
    ));
    let mut exchange = match send_request(context, &request) {
        Ok(exchange) => exchange,
        Err(error) => {
            return Ok(Err(json!({
                "outcome": "error",
                "kind": "transport",
                "reason": error.to_string(),
            })));
        }
    };

    let head_outcome = crate_step_in_page!(blobs.accept_list_head(exchange.response_head()));
    let outcome = match head_outcome {
        ListHeadOutcome::NeedErrorBody(failure) => {
            blobs.accept_list_error_body(failure.status, failure.request_id, &exchange.body)
        }
        outcome => outcome,
    };
    let failed_status = match outcome {
        ListHeadOutcome::Page { .. } => None,
        ListHeadOutcome::NotFound { .. } => Some(404),
        ListHeadOutcome::NeedErrorBody(failure) | ListHeadOutcome::ServiceFailure(failure) => {
            Some(failure.status)
        }
        _ => Some(exchange.status),
    };
    if let Some(status) = failed_status {
        return Ok(Err(exchange.error_result(status)));
    }

    // An array of `max_results` entries always holds a whole page.
    let slot_count = list_plan
        .max_results
        .map(|max_results| max_results as usize)
        .unwrap_or(AZURE_MAX_PAGE_ENTRIES);
    let mut slots = vec![ListEntry::default(); slot_count];
    let listing = crate_step_in_page!(blobs.fill_listing(&mut exchange.body, &mut slots));

    let metadata_requested = list_plan.include.contains(ListInclude::METADATA);
    let mut entries = Vec::new();
    let mut prefixes = Vec::new();
    for entry in &slots[..listing.filled] {
        match entry.kind {
            EntryKind::Prefix => prefixes.push(entry.key.to_owned()),
            _ => entries.push(listed_entry_value(entry, metadata_requested)),
        }
    }
    Ok(Ok(ListedPage {
        entries,
        prefixes,
        next_marker: listing
            .next_marker
            .filter(|marker| !marker.is_empty())
            .map(str::to_owned),
    }))
}

fn requested_page_size(call: &Value) -> Option<u32> {
    call.get("page_size")
        .and_then(Value::as_u64)
        .map(|page_size| page_size as u32)
}

fn list_page(
    context: &AdapterContext,
    blobs: &Blobs<'_>,
    call: &Value,
) -> Result<Value, AdapterError> {
    let mut include = ListInclude::default();
    for include_option in call
        .get("include")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match include_option.as_str() {
            Some("metadata") => include = include | ListInclude::METADATA,
            _ => return Ok(unsupported_by_crate("ListInclude names metadata only")),
        }
    }
    let delimited = match optional_text(call, "delimiter") {
        None => false,
        Some("/") => true,
        Some(_) => return Ok(unsupported_by_crate("PhysicalList delimits on '/' only")),
    };

    let list_plan = PhysicalList {
        prefix: optional_text(call, "prefix").unwrap_or_default(),
        marker: optional_text(call, "continuation_token"),
        delimited,
        max_results: requested_page_size(call),
        include,
    };
    Ok(match list_one_page(context, blobs, &list_plan)? {
        Ok(page) => successful_result(json!({
            "entries": page.entries,
            "prefixes": page.prefixes,
            "continuation_token": page.next_marker.unwrap_or_default(),
        })),
        Err(result) => result,
    })
}

fn list_all_keys(
    context: &AdapterContext,
    blobs: &Blobs<'_>,
    call: &Value,
) -> Result<Value, AdapterError> {
    let prefix = optional_text(call, "prefix").unwrap_or_default();
    let mut keys = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let list_plan = PhysicalList {
            prefix,
            marker: marker.as_deref(),
            delimited: false,
            max_results: requested_page_size(call),
            include: ListInclude::default(),
        };
        let page = match list_one_page(context, blobs, &list_plan)? {
            Ok(page) => page,
            Err(result) => return Ok(result),
        };
        keys.extend(page.entries.into_iter().map(|entry| entry["key"].clone()));
        match page.next_marker {
            Some(next_marker) => marker = Some(next_marker),
            None => break,
        }
    }
    Ok(successful_result(json!({"keys": keys})))
}

fn stage_block(
    context: &AdapterContext,
    blobs: &Blobs<'_>,
    call: &Value,
) -> Result<Value, AdapterError> {
    let key = optional_text(call, "key").unwrap_or_default();
    let block_id = optional_text(call, "block_id_base64").unwrap_or_default();
    let body = decode_base64_field(call, "body_base64")?;
    let stage_plan = PhysicalStageBlock {
        key,
        id: block_id,
        options: WriteOptions::default(),
    };
    let payload = Payload::Slice(&body);

    let now = current_timestamps();
    let (mut request_bytes, mut header_spans) = request_buffers(crate_step!(
        layered::stage_block_requirements(blobs, &stage_plan, payload, &now)
    ));
    let request = crate_step!(blobs.encode_stage_block(
        &mut request_bytes,
        &mut header_spans,
        &stage_plan,
        payload,
        &now
    ));
    let exchange = transport_step!(send_request(context, &request));

    let head_outcome = crate_step!(blobs.accept_stage_block_head(exchange.response_head()));
    let outcome = match head_outcome {
        StageBlockHeadOutcome::NeedErrorBody(failure) => {
            blobs.accept_stage_block_error_body(failure.status, failure.request_id, &exchange.body)
        }
        outcome => outcome,
    };

    Ok(match outcome {
        StageBlockHeadOutcome::Staged => successful_result(json!({})),
        StageBlockHeadOutcome::NotFound { .. } => exchange.error_result(404),
        StageBlockHeadOutcome::NeedErrorBody(failure)
        | StageBlockHeadOutcome::ServiceFailure(failure) => exchange.error_result(failure.status),
        _ => exchange.error_result(exchange.status),
    })
}

fn commit_blocks(
    context: &AdapterContext,
    blobs: &Blobs<'_>,
    call: &Value,
) -> Result<Value, AdapterError> {
    let Some((condition, condition_value)) = requested_condition(call) else {
        return Ok(unsupported_by_crate(
            "PhysicalCommitBlocks carries one precondition",
        ));
    };

    let mut block_references = Vec::new();
    for block in call
        .get("blocks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let source = match block.get("kind").and_then(Value::as_str) {
            Some("latest") => BlockSource::Latest,
            Some("committed") => BlockSource::Committed,
            Some("uncommitted") => BlockSource::Uncommitted,
            _ => return Ok(unsupported_by_adapter("block kind not mapped")),
        };
        block_references.push(BlockRef {
            id: block
                .get("id_base64")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            source,
        });
    }

    let key = optional_text(call, "key").unwrap_or_default();
    let commit_plan = PhysicalCommitBlocks {
        key,
        condition,
        condition_value,
        metadata: &[],
        options: WriteOptions {
            declared_md5: optional_text(call, "content_md5_base64"),
            ..WriteOptions::default()
        },
    };

    let now = current_timestamps();
    let (mut request_bytes, mut header_spans) = request_buffers(crate_step!(
        layered::commit_blocks_requirements(blobs, &commit_plan, &block_references, &now)
    ));
    let request = crate_step!(blobs.encode_commit_blocks(
        &mut request_bytes,
        &mut header_spans,
        &commit_plan,
        &block_references,
        &now
    ));
    let exchange = transport_step!(send_request(context, &request));

    let head_outcome =
        crate_step!(blobs.accept_commit_blocks_head(commit_plan.shape(), exchange.response_head()));
    let outcome = match head_outcome {
        CommitBlocksHeadOutcome::NeedErrorBody(failure) => blobs.accept_commit_blocks_error_body(
            commit_plan.shape(),
            failure.status,
            failure.request_id,
            &exchange.body,
        ),
        outcome => outcome,
    };

    Ok(match outcome {
        CommitBlocksHeadOutcome::Committed { meta, .. } => {
            let mut value = json!({"etag": text_of(meta.e_tag).unwrap_or_default()});
            if let Some(version) = text_of(meta.version) {
                value["version"] = json!(version);
            }
            successful_result(value)
        }
        CommitBlocksHeadOutcome::PreconditionFailed => exchange.error_result(exchange.status),
        CommitBlocksHeadOutcome::NotFound { .. } => exchange.error_result(404),
        CommitBlocksHeadOutcome::NeedErrorBody(failure)
        | CommitBlocksHeadOutcome::ServiceFailure(failure) => exchange.error_result(failure.status),
        _ => exchange.error_result(exchange.status),
    })
}

fn list_blocks(
    context: &AdapterContext,
    blobs: &Blobs<'_>,
    call: &Value,
) -> Result<Value, AdapterError> {
    let block_list_kind = match optional_text(call, "kind").unwrap_or("all") {
        "all" => BlockListKind::All,
        "committed" => BlockListKind::Committed,
        "uncommitted" => BlockListKind::Staged,
        _ => return Err("unknown block list kind".into()),
    };

    let key = optional_text(call, "key").unwrap_or_default();
    let list_blocks_plan = PhysicalListBlocks::new(key, block_list_kind);

    let now = current_timestamps();
    let (mut request_bytes, mut header_spans) = request_buffers(crate_step!(
        layered::list_blocks_requirements(blobs, &list_blocks_plan, &now)
    ));
    let request = crate_step!(blobs.encode_list_blocks(
        &mut request_bytes,
        &mut header_spans,
        &list_blocks_plan,
        &now
    ));
    let mut exchange = transport_step!(send_request(context, &request));

    let head_outcome = crate_step!(blobs.accept_list_blocks_head(exchange.response_head()));
    let outcome = match head_outcome {
        ListBlocksHeadOutcome::NeedErrorBody(failure) => {
            blobs.accept_list_blocks_error_body(failure.status, failure.request_id, &exchange.body)
        }
        outcome => outcome,
    };
    let failed_status = match outcome {
        ListBlocksHeadOutcome::Blocks { .. } => None,
        ListBlocksHeadOutcome::NotFound { .. } => Some(404),
        ListBlocksHeadOutcome::NeedErrorBody(failure)
        | ListBlocksHeadOutcome::ServiceFailure(failure) => Some(failure.status),
        _ => Some(exchange.status),
    };
    if let Some(status) = failed_status {
        return Ok(exchange.error_result(status));
    }

    // The crate bounds the count from the body's own length.
    let mut blocks = vec![Block::default(); layered::max_blocks_in(exchange.body.len())];
    let listing = crate_step!(blobs.fill_blocks(&mut exchange.body, &mut blocks));

    let mut committed = Vec::new();
    let mut uncommitted = Vec::new();
    for block in &blocks[..listing.filled] {
        let block_value = json!({"id_base64": block.id, "size": block.size});
        match block.state {
            BlockState::Committed => committed.push(block_value),
            _ => uncommitted.push(block_value),
        }
    }
    Ok(successful_result(json!({
        "committed": committed,
        "uncommitted": uncommitted,
    })))
}

fn compute_digest(call: &Value) -> Result<Value, AdapterError> {
    let body = decode_base64_field(call, "body_base64")?;
    let digest = match optional_text(call, "algorithm") {
        Some("md5") => {
            let mut checksum = Md5::default();
            checksum.update(&body);
            checksum.finish()
        }
        Some("azure_crc64") => {
            let mut checksum = Crc64::default();
            checksum.update(&body);
            checksum.finish()
        }
        _ => return Ok(unsupported_by_crate("digest algorithm")),
    };
    Ok(successful_result(
        json!({"digest_base64": STANDARD.encode(digest.as_bytes())}),
    ))
}

/// The account kind: from the endpoint, unless the command line names one.
/// `--namespace unknown` measures a client that was told nothing.
fn account_namespace(endpoint: &Value) -> AzureNamespace {
    let command_line_arguments: Vec<String> = std::env::args().collect();
    let named_namespace = command_line_arguments
        .windows(2)
        .find(|pair| pair[0] == "--namespace")
        .map(|pair| pair[1].clone());
    match named_namespace.as_deref() {
        Some("unknown") => AzureNamespace::Unknown,
        Some("flat") => AzureNamespace::Flat,
        Some("hierarchical") => AzureNamespace::Hierarchical,
        _ if endpoint
            .get("hierarchical_namespace")
            .and_then(Value::as_bool)
            .unwrap_or(false) =>
        {
            AzureNamespace::Hierarchical
        }
        _ => AzureNamespace::Flat,
    }
}

fn execute_operation(message: &Value) -> Result<Value, AdapterError> {
    if message.get("provider").and_then(Value::as_str) != Some("azure") {
        return Ok(unsupported_by_crate("the crate speaks Azure Blob only"));
    }
    let call = message.get("call").ok_or("missing call")?;
    CURRENT_CALL.set(call.clone()).ok();
    let operation = call.get("op").and_then(Value::as_str).ok_or("missing op")?;

    match operation {
        "digest" => return compute_digest(call),
        "hmac_sha256" | "azure.sign" | "azure.string_to_sign" => {
            return Ok(unsupported_by_crate(
                "the crate authenticates with a bearer token and implements no Shared Key signing",
            ));
        }
        "azure.snapshot" => return Ok(unsupported_by_crate("the crate has no snapshot operation")),
        _ => {}
    }

    let endpoint = message.get("endpoint").ok_or("missing endpoint")?;
    let is_live = message.get("mode").and_then(Value::as_str) == Some("live");
    let invalid_credentials =
        call.get("credential_mode").and_then(Value::as_str) == Some("invalid");
    let token = if !is_live {
        "offline-placeholder-token".to_owned()
    } else if endpoint.get("auth").and_then(Value::as_str) != Some("bearer") {
        return Ok(unsupported_by_crate(
            "the crate authenticates with a bearer token only",
        ));
    } else if invalid_credentials {
        "invalid".to_owned()
    } else {
        std::env::var("AZURE_STORAGE_ACCESS_TOKEN")?
    };

    let endpoint_url = optional_text(endpoint, "url").ok_or("missing endpoint url")?;
    let container_name = optional_text(endpoint, "bucket").ok_or("missing endpoint bucket")?;
    let namespace = account_namespace(endpoint);
    ACCOUNT_NAMESPACE.set(namespace).ok();
    let container = crate_step!(Container::new(endpoint_url, container_name));
    let blobs = crate_step!(Blobs::new(container, &token)).with_namespace(namespace);

    let mut agent_config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0);
    if let Some(proxy_url) = optional_text(endpoint, "proxy_url") {
        agent_config = agent_config.proxy(Some(ureq::Proxy::new(proxy_url)?));
    }
    let context = AdapterContext {
        http_agent: agent_config.build().into(),
    };

    match operation {
        "get" => read_object(&context, &blobs, call, GetKind::Bytes),
        "head" => read_object(&context, &blobs, call, GetKind::Head),
        "put" => write_object(&context, &blobs, call),
        "delete" => delete_object(&context, &blobs, call),
        "list" => list_all_keys(&context, &blobs, call),
        "list_page" => list_page(&context, &blobs, call),
        "azure.stage_block" => stage_block(&context, &blobs, call),
        "azure.commit_blocks" => commit_blocks(&context, &blobs, call),
        "azure.list_blocks" => list_blocks(&context, &blobs, call),
        _ => Ok(unsupported_by_adapter("operation not mapped")),
    }
}

fn main() {
    let mut input = String::new();
    let result = std::io::stdin()
        .read_to_string(&mut input)
        .map_err(AdapterError::from)
        .and_then(|_| Ok(serde_json::from_str::<Value>(&input)?))
        .and_then(|message| execute_operation(&message));

    match result {
        Ok(result) => println!("{result}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}
