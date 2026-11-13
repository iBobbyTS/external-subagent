# Native event contract fixtures

Every case records OBSERVED, SOURCE_INSPECTED or SYNTHETIC provenance. Expected
facts and dispositions are PLANNED adapter oracles, not captured ES output.
S03 validates the bounded sample shapes and contract expectations only; adapters
must consume these fixtures in their own regression tests before acceptance.
No model/network call was made to create this corpus. No fixture implies usage
or cost support, automatic retry, or full protocol coverage.

Error exemplars follow the inspected public App Server structs. The pinned
23934d31b1 structs were checked with git show (the reference checkout currently
points at e6bf88ae); these error fields agree at both commits.
They are not captured model failures. ErrorObserved does not settle a turn.
