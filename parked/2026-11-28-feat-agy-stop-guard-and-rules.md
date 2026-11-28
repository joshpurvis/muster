# Parked review findings: feat/agy-stop-guard-and-rules (28 Nov 2026)

One review (correctness + security): P1 0, P2 5, P3 4. All parked (internal repo, no P1).

- P2 loop: on a repeated Stop the adapter relies on agy's continuation message being in the transcript (agy 1.5.13 sends continuationActive false). If that append fails and a guard always blocks, agy is told to continue at every Stop. Fix if seen: a per-conversation continue counter in the session dir.
- P2 the Stop input lacks Claude Code's `last_assistant_message` and `permission_mode`; and `stop_hook_active` is false even when the continuation marker was seen. A guard that reads those fields errors, and the stop is allowed.
- P2 a symlinked `sessions/<id>` directory (replaced by a local process before restart) would redirect the rules copy and the Stop transcripts; the checks cover child paths only.
- P2 Claude Code's `disableAllHooks: true` is not honoured (same as for PreToolUse before this change).
- P2 a Claude model picked with agy's own `/model` (no `--model` on Muster's launch line) gets no rules copy.
- P3 a symlink in a rules source's parent path is followed (O_NOFOLLOW covers the last part only).
- P3 a killed hook leaves its `.agy-stop-*.jsonl` turn copy in the session dir until the tab's next start.
- P3 PreToolUse and Stop guards share the 1 MiB config read limit.
- P3 `MUSTER_ANTIGRAVITY_MCP=0` also turns off the Stop guards.
