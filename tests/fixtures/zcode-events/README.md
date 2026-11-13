# Native event contract fixtures

Every case records OBSERVED, SOURCE_INSPECTED or SYNTHETIC provenance. Expected
facts and dispositions are PLANNED adapter oracles, not captured ES output.
S03 validates the bounded sample shapes and contract expectations only; adapters
must consume these fixtures in their own regression tests before acceptance.
`cases.json` and `retry-log-observed.json` were built without a model/network
call. No fixture implies usage or cost support, automatic retry, or full
protocol coverage.

`retry-log-observed.json` is a bounded excerpt of native log evidence, not
captured wire. `cases.json` wire samples are SOURCE_INSPECTED exemplars built
from that evidence and the pinned v4 schema/notification producer. Terminal
telemetry is diagnostic only; it never authorizes TurnSettled. Native 429 and
retry waiting are consumed facts, not coverage damage or task failure.

`turn-lifecycle-shared-event-id.json` is OBSERVED wire: one real turn in which
the desktop UI channel repeats each fact under one native eventId across
`computer-use/operation-event`, `v4/telemetry/event` and `session/event`. It
pins the authoritative channel-precedence regression, not protocol coverage.
