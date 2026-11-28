# Parked review findings: fix/quiet-note-waits-for-depth (28 Nov 2026)

One review (correctness + security): P1 0, P2 1, P3 2. All parked.

- P2 a typed or pasted prompt quoting an older message's full header, for a message delivered to this session and not
  yet answered, is taken as that message's turn (lower hop, its origin). The daemon checks the id was delivered here
  and is unanswered, not that the injector submitted it this turn. Pre-existing for short prompts; this change only
  stops long messages from losing their header. Fix if seen: the daemon matches the id against its own last injection.
- P3 two separate paste blocks (an old message's opening tag ... a later closing tag) can parse as one wrapper; the
  daemon rejects the answered id but the turn then skips the human-input reset.
- P3 the replay test's hook transport follows the caller's `MUSTER_HOOK_DB`; run from inside a Muster session it can
  queue callbacks in the live inbox instead of the fixture daemon (scripts/isolated.sh clears it).
