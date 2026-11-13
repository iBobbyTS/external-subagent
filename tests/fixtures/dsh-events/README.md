# Native event contract fixtures

Every case records OBSERVED, SOURCE_INSPECTED or SYNTHETIC provenance. Expected
facts and dispositions are PLANNED adapter oracles, not captured ES output.
S03 validates the bounded sample shapes and contract expectations only; adapters
must consume these fixtures in their own regression tests before acceptance.
No model/network call was made to create this corpus. No fixture implies usage
or cost support, automatic retry, or full protocol coverage.

Standard ACP v1 chunk/tool/stopReason samples are source-inspected exemplars,
consistent with the existing dsh-acp fake server. They are not live DSH captures.
Settlement contains no message ID or committed flag. Tool title and kind differ.
