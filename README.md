# object-tests

Conformance cases for Azure Blob and S3 clients, and the grader that runs them. A case replays recorded HTTP exchanges on a loopback server, checks each request the client sends, and checks the result it reports.

## Grading

```sh
cargo build --release --locked
./target/release/object-tests grade cases/operations.json --provider azure -- ADAPTER [ARGS...]
```

| Suite | Contents |
|---|---|
| `core.json`, `operations.json`, `s3-express.json` | Operations over recorded HTTP |
| `vectors.json` | Digests and signatures |
| `live.json`, `s3-express-live.json` | Probes against a real account |

`--provider` selects the profiles of one provider and `--case ID` one case. Output is one JSON line per case, then the counts. The exit code is 0 when every case passes, 1 when any is wrong, failed or unsupported, and 2 for a configuration error.

## borink adapter

`adapters/borink` drives `borink-object-storage-proto` from the master branch of borink-org/object-storage.

```sh
cargo build --release --locked --manifest-path adapters/borink/Cargo.toml
./target/release/object-tests grade cases/operations.json --provider azure -- adapters/borink/target/release/borink-adapter
```

To build it against a checkout at `../object-storage` instead:

```sh
cargo build --release --manifest-path adapters/borink/Cargo.toml --config 'patch."https://github.com/borink-org/object-storage.git".borink-object-storage-proto.path="../object-storage/crates/object-storage-proto"' --config 'patch."https://github.com/borink-org/object-storage.git".borink-crypto.path="../object-storage/crates/crypto"'
```

The adapter tells the crate the account kind of the profile. `--namespace unknown` grades a client that was not told.

## Expected unsupported cases

A client can list the cases it expects to be unsupported, with the reason for each, by suite name:

```json
{"operations":{"operations/azure/get-snapshot":"PhysicalGet selects no snapshot or version"}}
```

```sh
./target/release/object-tests grade cases/operations.json --provider azure --expected-unsupported unsupported.json -- ADAPTER
```

The run then passes only when every listed case is unsupported and every other case passes. A regression, a wrong answer and a newly supported case each fail it, and a listed case the suite does not have is a configuration error. `--record-unsupported FILE` writes the list from a run instead, keeping the entries of cases the run did not grade.

## Adapter protocol

An adapter runs once per case. It reads one JSON input on stdin and writes one JSON result on stdout, within 30 seconds and 16 MiB.

```json
{"version":1,"mode":"offline","provider":"azure","endpoint":{"url":"http://127.0.0.1:PORT","account":"fixture","bucket":"fixture"},"call":{"op":"get","key":"literal%20"}}
```

Use the endpoint and dummy credentials given. If `endpoint.proxy_url` is set, send through it and keep `endpoint.url` for addressing and signing. Map `call` to the library and return what it exposes. Do not encode keys, repair the library, retry, or make extra calls.

```json
{"outcome":"ok","value":{"body_base64":"aGVsbG8=","etag":"\"opaque\""}}
{"outcome":"error","kind":"not_found","status":404,"code":"BlobNotFound"}
{"outcome":"unsupported","scope":"sdk","reason":"no snapshot selection"}
{"outcome":"refused","kind":"max_results","parameter":"page_size"}
{"outcome":"ok","value":{"etag":"\"opaque\""},"unsupported_fields":[{"at":"/value/content_md5_base64","scope":"sdk","reason":"not exposed"}]}
```

- Keys are unescaped and bodies are base64. A range is a `start` with an exclusive `end`, a `start` alone, or a `suffix` length.
- `list` returns every page; `list_page` returns one, with its entries, prefixes and continuation token.
- Only the properties a case asserts are required. A missing property the result declares in `unsupported_fields` makes the case unsupported; any other missing property makes it wrong.
- A refusal sends nothing. It passes only where the case has a `refusal`, which names the call parameter. Any status or code it gives must be the service's, and where the account kind decides the answer it must give both.
- `unsupported` passes only where the case has `decline_permitted`, because the service ignores the parameter.

## Cases

[`src/model.rs`](src/model.rs) defines the format. A case names a profile, a lane (`core`, `vectors` or `live`), the `call`, the HTTP `exchanges` and the `expect` checks on the result. Each exchange lists request alternatives with the response to send; an alternative's `then` exchanges must follow it. Unexpected requests fail.

A check addresses a JSON pointer with one rule: `equal`, `one_of`, `array_length`, `present`, `absent`, `matches`, `fresh` or `xml`. `optional: true` permits absence only.

## Live probes

Create an object named `object-tests-auth-probe` containing `authentication probe` and a newline, and put the endpoints in an untracked `*.live.local.json` file:

```json
{"azure":{"url":"https://ACCOUNT.blob.core.windows.net","account":"ACCOUNT","bucket":"CONTAINER"},"s3":{"url":"https://s3.REGION.amazonaws.com","region":"REGION","bucket":"BUCKET"}}
```

```sh
./target/release/object-tests live cases/live.json endpoints.live.local.json --provider azure -- ADAPTER
```

Azure reads `AZURE_STORAGE_KEY`, or `AZURE_STORAGE_ACCESS_TOKEN` with `"auth":"bearer"`. S3 reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and, if set, `AWS_SESSION_TOKEN`.
