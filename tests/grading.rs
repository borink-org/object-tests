use object_tests::{model::*, *};
use serde_json::{Value, json};
use std::time::{Duration, UNIX_EPOCH};

fn test_time() -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(1704067200)
}

// A few cases kept for the grader's own tests, apart from the corpus.
fn load_test_suite() -> Suite {
    Suite::load(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases.json")).unwrap()
}

fn decode_checks(value: Value) -> Vec<Check> {
    serde_json::from_value(value).unwrap()
}

fn create_azure_session(case: Case) -> Session {
    Session::new(case, load_test_suite().profiles["azure"].clone())
}

fn create_get_session() -> Session {
    create_azure_session(load_test_suite().cases.remove(0))
}

fn create_get_request() -> Value {
    normalize_http_request(
        "GET",
        "/fixture/object",
        &[("X-Ms-Version".into(), "2023-11-03".into())],
        b"",
    )
    .unwrap()
}

#[test]
fn corpus_is_strict_and_valid() {
    for file in [
        "operations.json",
        "vectors.json",
        "live.json",
        "s3-express.json",
        "s3-express-live.json",
    ] {
        Suite::load(format!("{}/cases/{file}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    }
    let mut suite_json = serde_json::to_value(load_test_suite()).unwrap();
    suite_json["cases"][0]["exchnages"] = json!([]);
    assert!(serde_json::from_value::<Suite>(suite_json).is_err());
}

#[test]
fn block_listing_assertions_check_order_sizes_and_array_types() {
    let suite = Suite::load(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/cases/operations.json"
    ))
    .unwrap();
    let case = suite
        .cases
        .iter()
        .find(|case| case.id == "operations/azure/staged-replacement")
        .unwrap();
    let correct = json!({
        "outcome": "ok",
        "value": {
            "committed": [
                {"id_base64": "MDAwMDAx", "size": 5},
                {"id_base64": "MDAwMDAy", "size": 6},
            ],
            "uncommitted": [{"id_base64": "MDAwMDAx", "size": 5}],
            "request_id": "an unrelated value",
        },
    });
    assert!(check_assertions(&case.expect, &correct, test_time()).is_empty());

    let mut reordered = correct.clone();
    reordered["value"]["committed"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    assert!(!check_assertions(&case.expect, &reordered, test_time()).is_empty());

    let mut wrong_size = correct.clone();
    wrong_size["value"]["uncommitted"][0]["size"] = json!(6);
    assert!(!check_assertions(&case.expect, &wrong_size, test_time()).is_empty());

    let mut object_instead_of_array = correct.clone();
    object_instead_of_array["value"]["uncommitted"] =
        json!({"0": {"id_base64": "MDAwMDAx", "size": 5}});
    assert!(!check_assertions(&case.expect, &object_instead_of_array, test_time()).is_empty());

    let mut extra_block = correct;
    extra_block["value"]["uncommitted"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id_base64": "MDAwMDAz", "size": 7}));
    assert!(!check_assertions(&case.expect, &extra_block, test_time()).is_empty());
}

#[test]
fn empty_array_assertion_requires_an_array() {
    let checks = [Check {
        at: "/entries".into(),
        optional: false,
        rule: Rule::ArrayLength { value: 0 },
        because: None,
    }];
    assert!(check_assertions(&checks, &json!({"entries": []}), test_time()).is_empty());
    for result in [json!({}), json!({"entries": {}}), json!({"entries": null})] {
        assert!(!check_assertions(&checks, &result, test_time()).is_empty());
    }
}

#[test]
fn a_raw_plus_in_the_query_fails_the_s3_profile_with_its_reason() {
    let profile = load_test_suite().profiles["s3"].clone();
    let request = |target: &str| normalize_http_request("GET", target, &[], b"").unwrap();

    let encoded = request("/fixture?list-type=2&continuation-token=a%2Bb");
    assert_eq!(encoded["query"]["continuation-token"], "a+b");
    assert!(check_assertions(&profile.checks, &encoded, test_time()).is_empty());

    let raw = request("/fixture?list-type=2&continuation-token=a+b");
    assert_eq!(raw["raw_query"], "list-type=2&continuation-token=a+b");
    let differences = check_assertions(&profile.checks, &raw, test_time());
    assert_eq!(differences.len(), 1);
    assert!(differences[0].because.as_deref().unwrap().contains("%2B"));
}

#[test]
fn duplicate_ids_and_empty_assertions_are_rejected() {
    let mut suite = load_test_suite();
    suite.cases.push(suite.cases[0].clone());
    assert!(suite.validate().is_err());
    suite.cases.pop();
    suite.cases[0].expect.clear();
    assert!(suite.validate().is_err());
}

#[test]
fn no_request_cannot_pass() {
    let session = create_get_session();
    assert_eq!(
        session.finish(
            &json!({
                "outcome": "ok",
                "value": {"body_base64": "aGVsbG8="},
            }),
            test_time()
        )["verdict"],
        "wrong"
    );
}

#[test]
fn actual_request_and_projected_result_pass() {
    let mut session = create_get_session();
    assert!(
        session
            .respond(&create_get_request(), test_time())
            .is_some()
    );
    assert_eq!(
        session.finish(
            &json!({
                "outcome": "ok",
                "value": {
                    "body_base64": "aGVsbG8=",
                    "request_id": "different",
                },
            }),
            test_time()
        )["verdict"],
        "pass"
    );
}

#[test]
fn wrong_request_poisons_later_correct_request() {
    let mut session = create_get_session();
    let mut request = create_get_request();
    request["path"] = json!("/fixture/wrong");
    assert!(session.respond(&request, test_time()).is_none());
    assert!(
        session
            .respond(&create_get_request(), test_time())
            .is_some()
    );
    assert_eq!(
        session.finish(
            &json!({
                "outcome": "ok",
                "value": {"body_base64": "aGVsbG8="},
            }),
            test_time()
        )["verdict"],
        "wrong"
    );
}

#[test]
fn extra_request_cannot_pass() {
    let mut session = create_get_session();
    session.respond(&create_get_request(), test_time());
    assert!(
        session
            .respond(&create_get_request(), test_time())
            .is_none()
    );
    assert_eq!(
        session.finish(&json!({"outcome": "unsupported"}), test_time())["verdict"],
        "wrong"
    );
}

#[test]
fn unknown_semantic_header_fails_but_user_agent_does_not() {
    let mut session = create_get_session();
    let mut request = create_get_request();
    request["headers"]["user-agent"] = json!("any SDK");
    assert!(session.respond(&request, test_time()).is_some());
    let mut session = create_get_session();
    request["headers"]["x-ms-delete-snapshots"] = json!("include");
    assert!(session.respond(&request, test_time()).is_none());
}

#[test]
fn missing_result_fields_are_not_silently_forgiven() {
    let mut session = create_get_session();
    session.respond(&create_get_request(), test_time());
    let report = session.finish(
        &json!({
            "outcome": "ok",
            "value": {},
        }),
        test_time(),
    );
    assert_eq!(report["verdict"], "wrong");
    assert_eq!(report["result_differences"][0]["at"], "/value/body_base64");
    // A wrong verdict carries the case's purpose, which states the rule it grades.
    assert!(
        report["purpose"]
            .as_str()
            .is_some_and(|purpose| !purpose.is_empty())
    );
}

#[test]
fn unsupported_is_never_a_pass() {
    assert_eq!(
        create_get_session().finish(
            &json!({
                "outcome": "unsupported",
                "reason": "no mapping",
            }),
            test_time()
        )["verdict"],
        "unsupported"
    );
}

#[test]
fn local_refusal_needs_an_explicit_case_and_reason() {
    let session = create_get_session();
    let refusal_result = json!({
        "outcome": "refused",
        "kind": "invalid_range",
    });
    assert_eq!(
        session.finish(&refusal_result, test_time())["verdict"],
        "wrong"
    );
    let mut case = load_test_suite().cases.remove(0);
    case.refusal = Some(decode_checks(json!([
        {
            "at": "/kind",
            "rule": {
                "is": "equal",
                "value": "invalid_range",
            },
        },
    ])));
    let session = create_azure_session(case);
    assert_eq!(
        session.finish(&refusal_result, test_time())["verdict"],
        "pass"
    );
    assert_eq!(
        session.finish(
            &json!({
                "outcome": "refused",
                "kind": "unknown",
            }),
            test_time()
        )["verdict"],
        "wrong"
    );
}

fn create_operations_session(case_id: &str) -> Session {
    let mut suite = Suite::load(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/cases/operations.json"
    ))
    .unwrap();
    let case_index = suite
        .cases
        .iter()
        .position(|case| case.id == case_id)
        .unwrap();
    let case = suite.cases.remove(case_index);
    let profile = suite.profiles[&case.profile].clone();
    Session::new(case, profile)
}

#[test]
fn declining_passes_where_the_service_ignores_the_listed_parameter() {
    let session = create_operations_session("operations/azure/get-suffix");
    let verdict_for = |result: Value| session.finish(&result, test_time())["verdict"].clone();

    assert_eq!(
        verdict_for(json!({
            "outcome": "unsupported",
            "reason": "no suffix option",
            "parameter": "range",
        })),
        "pass"
    );
    // An unsupported call that does not name the ignored parameter declined for another
    // reason, so it is unsupported, not a pass.
    assert_eq!(
        verdict_for(json!({"outcome": "unsupported", "reason": "no suffix option"})),
        "unsupported"
    );
    assert_eq!(
        verdict_for(json!({"outcome": "unsupported", "reason": "no reads", "parameter": "key"})),
        "unsupported"
    );
    assert_eq!(
        verdict_for(
            json!({"outcome": "refused", "kind": "unsupported_range", "parameter": "range"})
        ),
        "pass"
    );
    assert_eq!(
        verdict_for(json!({"outcome": "refused", "kind": "unsupported_range"})),
        "wrong"
    );
    assert_eq!(
        verdict_for(json!({"outcome": "refused", "kind": "unsupported_range", "parameter": "key"})),
        "wrong"
    );
    assert_eq!(
        verdict_for(json!({"outcome": "refused", "kind": "", "parameter": "range"})),
        "wrong"
    );
}

#[test]
fn listed_refusal_must_name_its_parameter_and_may_omit_but_not_contradict_the_answer() {
    let session = create_operations_session("operations/azure/list-max-keys-zero");
    let verdict_for = |result: Value| session.finish(&result, test_time())["verdict"].clone();

    assert_eq!(
        verdict_for(json!({"outcome": "refused", "kind": "max_results", "parameter": "page_size"})),
        "pass"
    );
    assert_eq!(
        verdict_for(json!({
            "outcome": "refused",
            "kind": "max_results",
            "parameter": "page_size",
            "status": 400,
            "code": "OutOfRangeQueryParameterValue",
        })),
        "pass"
    );
    assert_eq!(
        verdict_for(json!({
            "outcome": "refused",
            "kind": "max_results",
            "parameter": "page_size",
            "status": 404,
        })),
        "wrong"
    );
    assert_eq!(
        verdict_for(json!({"outcome": "refused", "kind": "max_results"})),
        "wrong"
    );
    assert_eq!(
        verdict_for(json!({"outcome": "refused", "kind": "invalid_prefix", "parameter": "prefix"})),
        "wrong"
    );
}

#[test]
fn unlisted_client_error_earns_nothing_for_a_refusal() {
    let session = create_operations_session("operations/azure/get-missing");
    assert_eq!(
        session.finish(
            &json!({"outcome": "refused", "kind": "not_found", "parameter": "key", "status": 404}),
            test_time()
        )["verdict"],
        "wrong"
    );
}

#[test]
fn account_specific_refusal_must_name_the_answer() {
    // A flat account refuses a 1,025-unit key and a hierarchical one stores it,
    // so only a client that names the flat account's answer may refuse it.
    let session = create_operations_session("operations/azure/put-key-1025");
    let verdict_for = |result: Value| session.finish(&result, test_time())["verdict"].clone();

    assert_eq!(
        verdict_for(json!({"outcome": "refused", "kind": "key_too_long", "parameter": "key"})),
        "wrong"
    );
    assert_eq!(
        verdict_for(json!({
            "outcome": "refused",
            "kind": "key_too_long",
            "parameter": "key",
            "status": 400,
            "code": "OutOfRangeInput",
        })),
        "pass"
    );
}

#[test]
fn declared_unsupported_field_is_a_limitation_but_a_silent_omission_is_wrong() {
    let create_session = || {
        let mut case = load_test_suite().cases.remove(0);
        case.expect.push(decode_checks(json!([
            {"at": "/value/content_md5_base64", "rule": {"is": "equal", "value": "XUFAKrxLKna5cZ2REBfFkg=="}},
        ])).remove(0));
        let mut session = create_azure_session(case);
        assert!(
            session
                .respond(&create_get_request(), test_time())
                .is_some()
        );
        session
    };
    let result_with = |unsupported_fields: Value| {
        json!({
            "outcome": "ok",
            "value": {"body_base64": "aGVsbG8="},
            "unsupported_fields": unsupported_fields,
        })
    };

    let silent_omission = create_session().finish(&result_with(json!([])), test_time());
    assert_eq!(silent_omission["verdict"], "wrong");

    let declared = create_session().finish(
        &result_with(json!([{
            "at": "/value/content_md5_base64",
            "scope": "sdk",
            "reason": "no Content-MD5 on a read",
        }])),
        test_time(),
    );
    assert_eq!(declared["verdict"], "unsupported");
    assert_eq!(declared["limitation_scope"], "sdk");
    assert_eq!(
        declared["unsupported_fields"],
        json!(["/value/content_md5_base64"])
    );

    let mut wrong_value = result_with(json!([{"at": "/value/content_md5_base64"}]));
    wrong_value["value"]["content_md5_base64"] = json!("AAAA");
    assert_eq!(
        create_session().finish(&wrong_value, test_time())["verdict"],
        "wrong"
    );
}

#[test]
fn dates_are_relative_to_execution_in_both_directions() {
    for (format, fresh, stale, future) in [
        (
            "http",
            "Mon, 01 Jan 2024 00:00:00 GMT",
            "Sun, 31 Dec 2023 23:00:00 GMT",
            "Mon, 01 Jan 2024 00:02:00 GMT",
        ),
        (
            "amz",
            "20240101T000000Z",
            "20231231T230000Z",
            "20240101T000200Z",
        ),
    ] {
        let assertions = decode_checks(json!([
            {
                "at": "/date",
                "rule": {
                    "is": "fresh",
                    "format": format,
                    "max_past_seconds": 900,
                    "max_future_seconds": 60,
                },
            },
        ]));
        assert!(check_assertions(&assertions, &json!({"date": fresh}), test_time()).is_empty());
        for bad in [stale, future, "garbage", "20240230T000000Z"] {
            assert!(!check_assertions(&assertions, &json!({"date": bad}), test_time()).is_empty());
        }
    }
}

#[test]
fn path_decodes_exactly_once_without_normalization() {
    let request =
        normalize_http_request("GET", "/a//../%2520/%2b?x=a+b&x=a%20b", &[], b"").unwrap();
    assert_eq!(request["path"], "/a//../%20/+");
    assert_eq!(request["query"]["x"], json!(["a+b", "a b"]));
    assert!(normalize_http_request("GET", "/%zz", &[], b"").is_err());
}

#[test]
fn duplicate_headers_do_not_disappear() {
    let request = normalize_http_request(
        "GET",
        "/",
        &[
            ("Range".into(), "bytes=1-2".into()),
            ("range".into(), "bytes=3-4".into()),
        ],
        b"",
    )
    .unwrap();
    assert_eq!(
        request["headers"]["range"],
        json!(["bytes=1-2", "bytes=3-4"])
    );
}

#[test]
fn binary_body_is_lossless() {
    let request = normalize_http_request("PUT", "/", &[], &[0, 255, 128]).unwrap();
    assert_eq!(request["body_base64"], "AP+A");
    assert!(request.get("body_text").is_none());
}

#[test]
fn xml_comparison_ignores_prefixes_but_preserves_order_and_values() {
    let assertions = decode_checks(json!([
        {
            "at": "/body",
            "rule": {
                "is": "xml",
                "value": "<a xmlns='urn:x'><b x='1'>a&amp;b</b><b>2</b></a>",
            },
        },
    ]));
    assert!(
        check_assertions(
            &assertions,
            &json!({
                "body": "<q:a xmlns:q='urn:x'>\n<q:b x=\"1\">a&#38;b</q:b><q:b>2</q:b></q:a>",
            }),
            test_time()
        )
        .is_empty()
    );
    for bad in [
        "<a><b>2</b></a>",
        "<a xmlns='urn:x'><b>2</b><b x='1'>a&amp;b</b></a>",
        "<a",
    ] {
        assert!(!check_assertions(&assertions, &json!({"body": bad}), test_time()).is_empty());
    }
}

#[test]
fn alternatives_choose_their_own_response() {
    let suite = load_test_suite();
    let case = suite
        .cases
        .iter()
        .find(|case| case.id == "azure/get-range-bounded")
        .unwrap();
    let mut session = Session::new(case.clone(), suite.profiles["azure"].clone());
    let mut request = create_get_request();
    request["headers"]["x-ms-range"] = json!("bytes=1-3");
    assert_eq!(session.respond(&request, test_time()).unwrap().status, 206);
}

#[test]
fn signature_mutation_fails_without_any_crypto_in_grader() {
    let suite = Suite::load(concat!(env!("CARGO_MANIFEST_DIR"), "/cases/vectors.json")).unwrap();
    let case = suite
        .cases
        .iter()
        .find(|case| case.id == "azure/shared-key-encoded-name")
        .unwrap();
    let Rule::Equal { value } = &case.expect[1].rule else {
        panic!()
    };
    let mut result = json!({
        "outcome": "ok",
        "value": {"authorization": value},
    });
    assert!(check_assertions(&case.expect, &result, test_time()).is_empty());
    result["value"]["authorization"] = json!(format!("{}0", value.as_str().unwrap()));
    assert!(!check_assertions(&case.expect, &result, test_time()).is_empty());
}

#[test]
fn absolute_request_targets_preserve_object_key_bytes() {
    let path = "/fixture/a//../b%3Fc?versionId=x%2By";
    let origin = normalize_http_request("GET", path, &[], b"").unwrap();
    for scheme in ["http", "https"] {
        let absolute =
            normalize_http_request("GET", &format!("{scheme}://localhost:1234{path}"), &[], b"")
                .unwrap();
        assert_eq!(origin, absolute);
    }
    assert_eq!(
        normalize_http_request("GET", "http://localhost?x=1", &[], b"").unwrap()["path"],
        "/"
    );
    for invalid in [
        "http:///object",
        "http://user@host/object",
        "/object#fragment",
        "object",
    ] {
        assert!(normalize_http_request("GET", invalid, &[], b"").is_err());
    }
}

#[test]
fn selected_branch_requires_its_follow_up_before_the_common_tail() {
    let mut case = load_test_suite().cases.remove(0);
    let base = case.exchanges[0].clone();
    let mut head = base.alternatives[0].clone();
    head.request
        .iter_mut()
        .find(|check| check.at == "/method")
        .unwrap()
        .rule = Rule::Equal {
        value: json!("HEAD"),
    };
    head.then = vec![base.clone()];
    case.exchanges[0].alternatives.push(head);
    let mut tail = base;
    tail.alternatives[0]
        .request
        .iter_mut()
        .find(|check| check.at == "/method")
        .unwrap()
        .rule = Rule::Equal {
        value: json!("DELETE"),
    };
    case.exchanges.push(tail);
    let mut session = create_azure_session(case.clone());
    let result = json!({
        "outcome": "ok",
        "value": {"body_base64": "aGVsbG8="},
    });
    let mut head_request = create_get_request();
    head_request["method"] = json!("HEAD");
    assert!(session.respond(&head_request, test_time()).is_some());
    assert_eq!(session.finish(&result, test_time())["verdict"], "wrong");
    assert!(
        session
            .respond(&create_get_request(), test_time())
            .is_some()
    );
    assert_eq!(session.finish(&result, test_time())["verdict"], "wrong");
    let mut delete_request = create_get_request();
    delete_request["method"] = json!("DELETE");
    assert!(session.respond(&delete_request, test_time()).is_some());
    assert_eq!(session.finish(&result, test_time())["verdict"], "pass");
    assert_eq!(
        session.finish(&result, test_time())["required_exchanges"],
        3
    );
    // Choosing the ordinary GET branch requires no extra exchange.
    let mut direct = create_azure_session(case);
    assert!(direct.respond(&create_get_request(), test_time()).is_some());
    assert!(direct.respond(&delete_request, test_time()).is_some());
    assert_eq!(direct.finish(&result, test_time())["verdict"], "pass");
    // Nested branches receive the same validation as top-level exchanges.
    let mut corpus = load_test_suite();
    corpus.cases[0].exchanges[0].alternatives[0].then = vec![Exchange {
        alternatives: vec![],
        optional: false,
    }];
    assert!(corpus.validate().is_err());
}

#[test]
fn a_client_may_stop_before_an_optional_trailing_exchange() {
    let mut case = load_test_suite().cases.remove(0);
    let mut resumed = case.exchanges[0].clone();
    resumed.optional = true;
    case.exchanges.push(resumed);
    let result = json!({"outcome": "ok", "value": {"body_base64": "aGVsbG8="}});

    let mut stopped = create_azure_session(case.clone());
    assert!(
        stopped
            .respond(&create_get_request(), test_time())
            .is_some()
    );
    let stopped_report = stopped.finish(&result, test_time());
    assert_eq!(stopped_report["verdict"], "pass");
    assert_eq!(stopped_report["required_exchanges"], 1);

    let mut continued = create_azure_session(case.clone());
    assert!(
        continued
            .respond(&create_get_request(), test_time())
            .is_some()
    );
    assert!(
        continued
            .respond(&create_get_request(), test_time())
            .is_some()
    );
    assert_eq!(continued.finish(&result, test_time())["verdict"], "pass");

    let mut suite = load_test_suite();
    let required = case.exchanges[0].clone();
    case.exchanges.push(required);
    suite.cases[0] = case;
    assert!(suite.validate().is_err());
}

#[test]
fn malformed_case_boundaries_are_rejected_before_execution() {
    let original = serde_json::to_value(load_test_suite()).unwrap();
    for (pointer, value) in [
        ("/profiles/azure/endpoint", json!(null)),
        ("/cases/0/call", json!([])),
        ("/cases/0/expect/0/at", json!("/value/~broken")),
        (
            "/cases/0/exchanges/0/alternatives/0/response/headers",
            json!({"x-test": "café"}),
        ),
        (
            "/cases/0/exchanges/0/alternatives/0/response/headers",
            json!({"content-length": "oops"}),
        ),
        (
            "/cases/0/exchanges/0/alternatives/0/response/headers",
            json!({
                "x-test": {
                    "from": "request",
                    "name": "contains space",
                },
            }),
        ),
    ] {
        let mut value_to_check = original.clone();
        *value_to_check.pointer_mut(pointer).unwrap() = value;
        let suite: Suite = serde_json::from_value(value_to_check).unwrap();
        assert!(suite.validate().is_err(), "{pointer}");
    }
}

#[test]
fn response_header_names_allow_http_tokens_and_reject_case_duplicates() {
    let mut suite = load_test_suite();
    let headers = &mut suite.cases[0].exchanges[0].alternatives[0].response.headers;
    headers.insert(
        "x-test_field".into(),
        Header::Dynamic(DynamicHeader::Request {
            name: "X-Client-Id".into(),
        }),
    );
    assert!(suite.validate().is_ok());
    suite.cases[0].exchanges[0].alternatives[0]
        .response
        .headers
        .insert("X-Test_Field".into(), Header::Literal("duplicate".into()));
    assert!(suite.validate().is_err());
}
