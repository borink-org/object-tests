# object-tests

**Warning: currently this is still unstable and experimental and the Git history is also unstable, expect it to be rewritten and commits to no longer resolve!**

If you write an object storage client, you want to be sure it works with the real Azure Blob Storage and AWS S3. However, exactly matching their behaviors and knowing for sure you support all the features you want isn't easy! This project tries (but does not yet fully succeed) to make it easier. It records a large number of "cases", which are generated once against live Azure Blob and S3, which are used by a local HTTP server that you can then run your client against as if it's an object storage provider. This way we don't need to _reimplement_ S3/Azure Blob but can instead just copy their behavior on a predefined set of tests.

The published code here is only the actual cases (in JSON format) and the grader, as well as an example adapter (because each JSON case has to be then actually turned into the right operations on your client, which of course cannot be done fully mechanically, although your clanker should be able to mostly do the work for you, at least that's the theory) for the client that this suite was designed for, [borink-object-storage](https://github.com/borink-org/object-storage).

There are also a few live tests that are just to see if you can actually make a valid request with credentials. We also include some useful vectors for testing the crypto parts of your object storage client.

| Suite | Contents |
|---|---|
| `operations.json` | Object operations over recorded HTTP |
| `management.json` | Bucket and container operations over recorded HTTP |
| `vectors.json` | Digests and signatures |
| `live.json` | Probes against a live service |
| `large.json` | Size limits and large reads, with bodies of gigabytes that the grader generates; graded only when asked for |

The Azure cases authorize with Microsoft Entra ID only. The suite does not aim to test shared access signatures (SAS) or Shared Key, and has no cases or vectors for them.

[REFERENCE.md](REFERENCE.md) describes the adapter protocol.

## Example test of `borink-object-storage`

`adapters/borink` builds the adapter that borink-org/object-storage keeps in [`hosts/object-tests`](https://github.com/borink-org/object-storage/tree/master/hosts/object-tests), at the commit its `Cargo.toml` pins. That adapter lives beside the crates whose API it calls, and object-storage grades every change against this suite.

```bash
cargo build --release --locked
cargo build --release --locked --manifest-path adapters/borink/Cargo.toml
./target/release/object-tests grade cases/operations.json --provider azure -- adapters/borink/target/release/borink-adapter
```

The compilation takes longer than running the tests! It currently just dumps a large JSON.

The adapter reports some cases unsupported, and without a list of those the grader fails the run. `--record-unsupported FILE` writes the list and fails only on a case the adapter gets wrong; `--expected-unsupported FILE` then holds a later run to it.

The grader sends a protocol `version` in every message, and the adapter refuses one it does not speak. A change to the protocol bumps it, and object-storage updates its adapter before this example moves its pin.

## LLM disclaimer

This project is heavily AI-assisted. It's mostly a generated artifact from a private repo that also contains the code for actually creating all of the cases. The individual cases and none of the code has been reviewed in depth (this differs quite a bit from the other projects under the `borink-org` umbrella). The slop level is almost certainly quite high, which should be fine as this is a verification artifact that doesn't prove the absence of bugs, it can only prove that maybe your client has some bugs. And if it has false positives, we can easily fix those. Remember: you have been warned!
