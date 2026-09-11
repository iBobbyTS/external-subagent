# DeepSeek Harness ACP compatibility

This document records the bounded S01 probe contract. It is not a claim that DSH is supported by the product.

The probe is `tools/probes/dsh-acp/probe.mjs`. It accepts an explicit executable path and argv, speaks JSON-RPC over stdio, and never installs a provider or writes credentials. `--help` lists the only scenarios. The caller must supply the exact executable and arguments obtained from its installed DSH version; the probe does not invent `dsh app-server`, login, or profile commands.

Observed fixture shapes cover `initialize`, `session/new`, `models/list`, `session/prompt`, `session/update`, `session/request_permission`, `session/cancel`, EOF, and malformed input. `models/list` is catalog evidence only. ACP `initialize` success is not provider authentication evidence; a real auth/hi result must be recorded separately for the exact executable, environment, workspace, and configuration revision.

The product's explicit `agent_models` RPC consumes only the already observed `initialize`, `session/new`, and `models/list` shapes. It returns model IDs as opaque catalog tokens and always cleans up its bounded discovery process. Catalog success does not enable DSH production spawn or imply provider authentication.

S01 live status: **BLOCKED / NOT_RUN** until an authorized DSH executable, version/source, runtime, credentials, and disposable workspace are supplied. The local fixture is deterministic protocol coverage, not a substitute for a real DSH run. Strict-plan enforcement and complete write/shell entry-point coverage remain unproven; therefore no DSH spawn support or policy plugin is enabled by this section.
