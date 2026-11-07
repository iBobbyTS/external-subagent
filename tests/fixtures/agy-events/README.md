# Native event contract fixtures

Every case records OBSERVED, SOURCE_INSPECTED or SYNTHETIC provenance. Expected
facts and dispositions are PLANNED adapter oracles, not captured ES output.
S03 validates the bounded sample shapes and contract expectations only; adapters
must consume these fixtures in their own regression tests before acceptance.
No model/network call was made to create this corpus. No fixture implies usage
or cost support, automatic retry, or full protocol coverage.

`stream-observed.json` retains all eight stream-json frames from AGY 1.5.0.
`acp-observed.json` retains the other observed probe for comparison only. ACP is
not a supported AGY inbound protocol in this feature and has no adapter
classification expectation. Source SHA-256 values refer to untouched originals.
UUIDs/cwd are substituted as declared. Tool DONE supplies Finished/Unknown;
output prose and usage thinking_tokens are not success/reasoning evidence.
