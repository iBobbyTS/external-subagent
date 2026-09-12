# DeepSeek Harness ACP compatibility

This document records the bounded S01 probe contract. It is not a claim that DSH is supported by the product.

The probe is `tools/probes/dsh-acp/probe.mjs`. It accepts an explicit executable path and argv, speaks JSON-RPC over stdio, and never installs a provider or writes credentials. `--help` lists the only scenarios. The caller must supply the exact executable and arguments obtained from its installed DSH version; the probe does not invent `dsh app-server`, login, or profile commands.

Observed fixture shapes cover `initialize`, `session/new`, `models/list`, `session/prompt`, `session/update`, `session/request_permission`, `session/cancel`, EOF, and malformed input. `models/list` is catalog evidence only. ACP `initialize` success is not provider authentication evidence; a real auth/hi result must be recorded separately for the exact executable, environment, workspace, and configuration revision.

The product's explicit `agent_models` RPC consumes only the already observed `initialize`, `session/new`, and `models/list` shapes. It returns model IDs as opaque catalog tokens and always cleans up its bounded discovery process. Catalog success does not enable DSH production spawn or imply provider authentication.

Catalog discovery isolates the provider in a process group and cleans the group with TERM/KILL plus leader wait on success and failure. Protocol stdout uses an incremental 1 MiB frame cap, and diagnostic stderr uses a 64 KiB bounded reader with a bounded receive deadline; descendants inheriting stderr cannot hold discovery open.

The shared executable-version check applies the same process-group and incremental-reader bounds, including cleanup when a leader exits while a descendant still owns stdout/stderr.

S04 live status (2026-09-12): **PARTIALLY VERIFIED** against DSH `0.1.5-rc.1` in a disposable workspace. Real ACP `initialize`, `session/new`, and `session/prompt` completed; the default-model prompt returned `LIVE_OK` with `end_turn`. Under the strict patch profile, shell/write probing exposed only the expected `glob`/`grep` operations and created no target file.

These observations validate the ACP wire path and the bounded strict-plan probe. Production daemon routing is implemented behind the explicit `enabled + spawn_supported + DSH_RUNTIME_PATH` gate, but a daemon-routed live lifecycle (spawn, prompt, cancellation, cleanup, and restart), provider `hi`/authentication, and Codex/ZCode host integration remain **NOT_RUN**. The default configuration remains closed; this document does not claim that a live production deployment is enabled.
