import { inheritedProfile, parseProfileChoice, profileName } from "./account_profiles.ts";
import type { AccountProfile, SessionInfo } from "./types.ts";

function equal(actual: unknown, expected: unknown) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`Expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

Deno.test("Claude splits inherit the source profile even when workspace defaults differ", () => {
  const source = { kind: "claude", account_profile_id: 7 } as SessionInfo;
  equal(inheritedProfile("claude", source), 7);
  equal(inheritedProfile("shell", source), undefined);
  equal(inheritedProfile("codex", source), undefined);
  equal(inheritedProfile("claude", { kind: "codex" } as SessionInfo), undefined);
  equal(inheritedProfile("claude", undefined), undefined);
});

Deno.test("a default-login Claude parent explicitly pins Default instead of inheriting a new workspace default", () => {
  equal(inheritedProfile("claude", { kind: "claude", account_profile_id: null } as SessionInfo), "default");
  equal(inheritedProfile("claude", { kind: "claude" } as SessionInfo), "default");
  equal(parseProfileChoice(""), undefined);
  equal(parseProfileChoice("default"), "default");
  equal(parseProfileChoice("9"), 9);
});

Deno.test("missing or disabled profile bindings stay visible instead of appearing as Default", () => {
  const profiles = [{ id: 4, name: "Work", enabled: false }] as AccountProfile[];
  equal(profileName(profiles, 4), "Work");
  equal(profileName(profiles, 5), "Unavailable profile #5");
  equal(profileName(profiles, null), "Default");
  for (const value of ["0", "-1", "1.5", "NaN", "personal", "9007199254740992"]) {
    let rejected = false;
    try {
      parseProfileChoice(value);
    } catch {
      rejected = true;
    }
    equal(rejected, true);
  }
});
