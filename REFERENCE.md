# Grader reference

```nu
./target/debug/object-tests grade cases/operations.json --profile s3 -- ADAPTER
```

`--case`, `--provider` and `--profile` select cases. `--expected-unsupported FILE` lists cases, patterns with `*`, or `field:/value/...` result fields that may be unsupported; `--record-unsupported FILE` writes that list. Exit code `0` means every case passed.

## Adapter protocol

The grader starts the adapter once per case, writes one JSON input to its stdin and reads one JSON result from its stdout:

```json
{"version":1,"mode":"offline","provider":"azure","endpoint":{"url":"http://127.0.0.1:PORT","account":"fixture","bucket":"fixture"},"call":{"op":"get","key":"a b"}}
```

Use `endpoint` with dummy credentials, and `endpoint.proxy_url` as the HTTP proxy where given. Map `call` to the client and report what it returns. Do not encode keys, repair client behavior or make extra calls.

```json
{"outcome":"ok","value":{"body_base64":"aGVsbG8="}}
{"outcome":"error","kind":"not_found","status":404,"code":"BlobNotFound"}
{"outcome":"refused","kind":"invalid_range","parameter":"range"}
{"outcome":"unsupported","reason":"not mapped","parameter":"range"}
```

- `error`: the service refused the call.
- `refused`: the client refused it before sending.
- `unsupported`: the client cannot make it. Naming a `parameter` declines that parameter, which some cases permit.

An error names a `kind`. `not_found` means the object is absent, `container_not_found` the bucket or container, and `missing` that the answer cannot say which. The others are `precondition`, `not_modified`, `permission_denied` and `other`.

Leave out a result property the client does not expose, and name it in `unsupported_fields`, as `[{"at":"/value/content_md5_base64","reason":"..."}]`.

Keys are unescaped, bodies base64, ranges an offset and an exclusive end. A `checksum` without `value_base64` is for the client to compute. Tags are an object, or a list of `{"key", "value"}` pairs where a key may repeat. `content_range` reports a served range as `bytes FIRST-LAST/SIZE`.

A `delete_many` key is a name or `{"key", "version"}`, on Azure also `{"key", "snapshot"}`. A result reports each key as the call named it. Where an S3 answer names a version or a delete marker, the key is an object with `version`, `delete_marker` and `delete_marker_version`. A version listing pages by `continuation_token` and `version_marker`, and returns `next_version_marker`. A `restore` reports `state` `started` for a 202 and `readable` for a 200, and a read reports `x-amz-restore` or `x-ms-archive-status` as `restore_status`.

## Cases

[model.rs](src/model.rs) defines the format.

- `origin` says where the responses come from: `observed` (the default), `transient` (sent, but not on demand) or `constructed` (written by hand). `derived_from` names the recorded case such a case edits.
- `refusal` holds the checks a refusal must pass; a case without it accepts none. `decline_permitted` lets an `unsupported` result that names the parameter pass.
- Checks address JSON pointers. `matches` is a whole-string regex, `xml` compares structure, and `optional` permits absence only. `blob_batch` and `body_checksum` apply at the request's root.
- A request header's value holds one character per byte, so `é` is the byte e9 and `Ã©` its UTF-8.
