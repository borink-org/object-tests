# Grader reference

The reference for the grader, its options, the adapter protocol and the case format. The [README](README.md) is a short introduction; this file keeps the detail.

JSON conformance cases for Azure Blob and S3 clients. Core cases and signing vectors run offline through a loopback HTTP server; live probes require credentials.

```nu
cargo build --locked
./target/debug/object-tests validate cases/operations.json
./target/debug/object-tests grade cases/operations.json --provider azure -- /path/to/azure-cpp
./target/debug/object-tests grade cases/operations.json --profile s3 -- /path/to/aws-s3-crt-adapter
./target/debug/object-tests grade cases/management.json --profile s3 -- /path/to/aws-s3-crt-adapter
./target/debug/object-tests grade cases/vectors.json --provider s3 -- /path/to/aws-c-s3-adapter
```

`adapters/` holds the supplied adapters, each with its own build.

`--case ID` selects one case, `--provider` the cases of one service and `--profile` those of one profile, such as `s3` or `s3-express`. `--jobs N` grades N cases at once instead of one per CPU. Output is JSON lines: case verdicts followed by lane counts. Exit codes: `0` all passed, `1` wrong/failed/unsupported, `2` configuration error.

## Suites

| Suite | Contents |
|---|---|
| `operations.json` | Object operations over recorded HTTP |
| `management.json` | Bucket and container operations, such as CreateBucket, PutBucketVersioning and Create Container |
| `vectors.json` | Digests and signatures |
| `live.json` | Probes against a live service |

A suite contains `version`, `index`, `profiles` and `cases`. The `index` maps each case ID to its purpose, so that a reader can survey the cases before their rules.

## Expected unsupported cases

A client can list the cases it expects to be unsupported, with the reason for each, by suite name:

```json
{"operations":{"operations/azure/get-snapshot":"PhysicalGet selects no snapshot or version"}}
```

```nu
(
  ./target/debug/object-tests grade cases/operations.json
  --provider azure
  --expected-unsupported unsupported.json
  -- ADAPTER
)
```

The run then passes only when every listed case is unsupported and every other case passes. A regression, a wrong answer and a newly supported case each fail it.

- An entry may be a pattern, in which `*` matches any run of characters: `"operations/s3/*"` covers every S3 case of the operations suite, and `"operations/azure*/lease-*"` every lease case of either Azure profile.
- An entry may name a result field, as `"field:/value/content_md5_base64"`, for a client that cannot report it. It covers every case that the result made unsupported only by leaving out declared fields that the list names this way.
- Every case an entry covers must be unsupported. An entry that covers no case of the suite is a configuration error.

`--record-unsupported FILE` writes the list from a run instead. It keeps patterns and field entries, which you write by hand, and lists only the unsupported cases that none of them covers.

## Adapter protocol

One process per case: one JSON input on stdin, one JSON result on stdout, diagnostics on stderr. Limits: 30 seconds, 16 MiB requests and output.

### Input

```json
{"version":1,"mode":"offline","provider":"azure","endpoint":{"url":"http://127.0.0.1:PORT","account":"fixture","bucket":"fixture"},"call":{"op":"get","key":"literal%20"}}
```

- Configure the supplied endpoint and dummy credentials offline.
- If `endpoint.proxy_url` is supplied, use it as the HTTP proxy, and keep `endpoint.url` for addressing and signing. An offline adapter also finds the proxy in `HTTP_PROXY` and `http_proxy`.
- An S3 Express profile names a directory bucket with `endpoint.directory_bucket`, and in `management.json` its regional control endpoint in `endpoint.control_url`.
- Map `call` to SDK methods and return the properties the SDK exposes. Disable credential discovery and retries where supported.
- Do not encode keys in the adapter, repair SDK behavior or make extra calls to obtain missing properties.

### Results

```json
{"outcome":"ok","value":{"body_base64":"aGVsbG8=","etag":"\"opaque\""}}
{"outcome":"error","kind":"not_found","status":404,"code":"BlobNotFound"}
{"outcome":"refused","kind":"invalid_range","parameter":"range"}
{"outcome":"unsupported","reason":"not mapped","scope":"adapter"}
```

- `ok`: the call succeeded, and `value` holds its result.
- `error`: the service refused the call. It names the HTTP `status`, the service's `code` where the answer gives one, and a `kind`.
- `refused`: the client refused the call before sending. It names a `kind` and the call field it objects to as `parameter`.
- `unsupported`: the client cannot make the call. A result that names a `parameter` declines that parameter, which a case may permit.

The `kind` of an error:

| Kind | Meaning |
|---|---|
| `not_found` | The object is absent |
| `container_not_found` | The bucket or container is absent |
| `missing` | Something is absent, and the answer cannot say what, as for an S3 HEAD answered 404 without a body |
| `precondition` | A condition failed, 412 |
| `not_modified` | 304 to a condition on a read |
| `permission_denied` | 401 or 403 |
| `other` | Anything else |

Only properties asserted by a case are required. Leave out a property that the SDK does not expose, and declare it in `unsupported_fields`, as in `[{"at":"/value/content_md5_base64","scope":"sdk","reason":"..."}]`. A case that misses only declared properties is `unsupported`; one that misses an undeclared property is `wrong`.

### Calls

The cases show every call in full. These conventions hold across them:

- **Values.** Keys are unescaped. Bodies are base64, in `body_base64`. Conditions are opaque ETag strings, and dates are HTTP dates.
- **Ranges.** `range` and `source_range` are an offset and an exclusive end, `{"start": 28, "end": 64}`, an offset alone, or a suffix length, `{"suffix": 5}`.
- **Listings.** `list` consumes every page; `list_page` returns one page, its entries, prefixes and continuation token. `page_size` maps to the SDK's page limit, and `start_after` and `fetch_owner` to S3's `start-after` and `fetch-owner`.
- **Checksums.** A write names a checksum in `checksum`, with its `algorithm` and `value_base64`. Without `value_base64`, the client computes it from the body, and may send it in a header or in the trailer of an `aws-chunked` body. `checksums` names several, which the service refuses.
- **Tags.** Tags are an object, or a list of `{"key": ..., "value": ...}` pairs where order or a repeated key matters.
- **Content properties.** `content_type`, `content_encoding`, `content_language`, `content_disposition` and `cache_control` on a write; `tier` on Azure and `storage_class` on S3.
- **Copies.** A `copy` names its source in `source_key`. `source_bucket` on S3 or `source_container` on Azure names a source elsewhere, and `source_version` an earlier version. `source_if_match`, `source_if_none_match`, `source_if_modified_since` and `source_if_unmodified_since` are conditions on the source. `metadata`, `content_type`, `tags`, `storage_class` and `tier` on a copy replace the source's. `sync` asks Azure for Copy Blob From URL.
- **Provider operations.** Operations of one provider are named explicitly:
  - Azure: `azure.stage_block`, `azure.stage_block_from_url`, `azure.put_from_url`, and `azure.abort_copy` with `copy_id`.
  - S3: `s3.upload_part`, `s3.upload_part_copy`, `s3.put_bucket_versioning` with `status`, and `s3.create_bucket` with `availability_zone` for a directory bucket.

### Result fields

| Field | Holds |
|---|---|
| `body_base64`, `size` | The bytes read and their count |
| `etag`, `version`, `snapshot` | Identifiers the service gave |
| `content_type`, `content_encoding`, `content_language`, `content_disposition`, `cache_control` | Content properties |
| `content_md5_base64` | The stored MD5 |
| `metadata`, `tags` | Objects of names and values |
| `storage_class` | An S3 storage class, as S3 names it |
| `access_tier` | An Azure access tier, from `x-ms-access-tier` |
| `content_range` | The window a ranged read served, as `bytes FIRST-LAST/SIZE` |
| `copy_status`, `copy_id` | The state of an Azure copy, from a copy or a HEAD |
| `deleted`, `errors` | A batch delete's deleted keys, and its failures by `key`, `code` and Azure `status` |
| `upload_id`, `parts` | A multipart upload and its parts |
| `entries`, `prefixes`, `continuation_token`, `keys` | A listing |

A listed entry reports `last_modified` in RFC 3339 in UTC with whole seconds, as `2024-01-02T03:04:05Z`. It reports `storage_class` as the service names it, `checksum_algorithm` as one name, such as `CRC32`, and `owner_id`.

## Cases

[Cases](cases/) are executable examples; [model.rs](src/model.rs) defines the schema. Each case declares its lane (`core`, `vectors`, `live`), `call`, HTTP `exchanges` and result `expect` checks.

### Where the responses come from

A case may declare its `origin`: how we know that a service sends its responses.

- `observed`, the default and left out, means the service was seen to send them.
- `transient` means the service sends them, but not on demand, such as a 503 under load or a body a network fault cuts short.
- `constructed` means no service is known to send them. Such a case tests what a client does with a response that the protocol allows or a faulty service might send.

A case that is not observed may name `derived_from`: the recorded case whose responses it edits by hand. The grader grades every origin alike; the origin says how far a verdict reaches.

### Exchanges and refusals

Each exchange has request and response `alternatives`; optional `then` exchanges are required when that alternative matches. Unexpected traffic, missing exchanges and missing asserted result fields fail. Unsupported operations earn no pass.

A response may carry `body_time`, as `{"text": "SESSION_EXPIRATION", "offset_seconds": 300}`. The grader replaces that text in the body with the time that many seconds after it sends the response, in ISO 8601 UTC.

A refusal passes only in a case that declares `refusal` checks, and only with no traffic:

- Such a case is one whose 4xx or 501 answer the call alone determines.
- It may also be one that sends a value encoded, such as S3 metadata outside ASCII. There the service refuses the plain bytes or stores other text.
- The refusal names a `kind` and the call field it objects to as `parameter`. Any `status` or `code` it names must be the service's.
- Where a flat and a hierarchical Azure account answer differently, the refusal must name both, since only a client told the account kind can know.

`decline_permitted` marks a case whose parameter a client may decline. The service may ignore it, as a range it cannot serve. Or a client API may be unable to hold it, as a repeated tag key. Reporting such a case `unsupported` before sending passes, if the result names that parameter as `parameter`.

### Checks

Checks address JSON pointers and use these rules:

| Rule | Fields |
|---|---|
| `equal` | `value` |
| `one_of` | `values` |
| `array_length` | `value` |
| `present`, `absent` | — |
| `matches` | `pattern` (whole-string regex) |
| `fresh` | `format`, `max_past_seconds`, `max_future_seconds` |
| `xml` | `value` (structural XML comparison) |
| `blob_batch` | `subrequests`, each a `method` and a decoded `path` |
| `body_checksum` | `header`, `algorithm` (`crc32`, `crc32c` or `crc64nvme`) |

`blob_batch` and `body_checksum` apply at the request's root, `"at": ""`, and hold Rust code where a pattern would not do. `blob_batch` checks an Azure Blob Batch body: the boundary its content type names, CRLF line ends and the closing delimiter. Each part must be `application/http` in `binary`, with a Content-ID of its own. It holds one subrequest with `x-ms-date` and `Authorization` and no body. The subrequests may come in any order. `body_checksum` checks that a header holds the checksum of the body that the client sent.

`optional: true` permits absence only. A check may give `because`, the reason the service needs it, which a failure of the check reports.

Request fields are `method`, `raw_path`, once-decoded `path`, `raw_query` as sent, decoded `query`, lowercase `headers`, `body_base64` and, for UTF-8, `body_text`. A header value holds one ISO-8859-1 character per byte, so `é` is the byte e9 and `Ã©` is the UTF-8 of é. Duplicate headers and query fields become arrays. Query `+` stays literal. Unknown headers and query keys require explicit `allow_headers` and `allow_query` patterns.

Response bodies use `empty`, `utf8` or `base64` encoding. Headers may be literal, `{ "from": "now" }`, or `{ "from": "request", "name": "x-ms-client-request-id" }`. XML comparisons preserve namespaces, child order, duplicate elements and text.

## Live probes

Create a private `object-tests-auth-probe` object containing `authentication probe\n` (a trailing newline). Put endpoints in an untracked `*.live.local.json` file:

```json
{
  "azure":{"url":"https://ACCOUNT.blob.core.windows.net","account":"ACCOUNT","bucket":"CONTAINER"},
  "s3":{"url":"https://s3.REGION.amazonaws.com","region":"REGION","bucket":"BUCKET"}
}
```

```nu
./target/debug/object-tests live cases/live.json endpoints.live.local.json --provider azure -- /path/to/azure-cpp
./target/debug/object-tests live cases/live.json endpoints.live.local.json --profile s3 -- /path/to/aws-s3-crt-adapter
```

Azure uses `AZURE_STORAGE_KEY`, or endpoint `auth: "bearer"` with `AZURE_STORAGE_ACCESS_TOKEN`. S3 uses `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optional `AWS_SESSION_TOKEN`. Keep credentials out of JSON.

The supplied probes perform successful and invalid-credential reads; they do not write or delete. The `s3-express` probes run the S3 probes against a directory bucket: give their endpoint, under `s3-express`, `directory_bucket: true`, the directory bucket name and `url: "https://s3express-ZONE.REGION.amazonaws.com"`. They do not cover token refresh, SAS or presigned URLs.
