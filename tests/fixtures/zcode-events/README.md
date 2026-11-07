# Native event contract fixtures

Every case records OBSERVED, SOURCE_INSPECTED or SYNTHETIC provenance. Expected
facts and dispositions are PLANNED adapter oracles, not captured ES output.
S03 validates the bounded sample shapes and contract expectations only; adapters
must consume these fixtures in their own regression tests before acceptance.
No model/network call was made to create this corpus. No fixture implies usage
or cost support, automatic retry, or full protocol coverage.

`retry-log-observed.json` is a bounded excerpt of native log evidence, not
captured wire. `cases.json` wire samples are SOURCE_INSPECTED exemplars built
from that evidence and the pinned v4 schema/notification producer. Terminal
telemetry is diagnostic only; it never authorizes TurnSettled. Native 429 and
retry waiting are consumed facts, not coverage damage or task failure.
