import type { AccountProfile, ProfileChoice, SessionInfo } from "./types.ts";

export function profileName(profiles: AccountProfile[], id: number | null | undefined): string {
  return id == null ? "Default" : profiles.find((p) => p.id === id)?.name ?? `Unavailable profile #${id}`;
}

/** A new Claude tab/split follows the source Claude session, including its explicit Default.
 * A shell/Codex source leaves selection to the workspace default. */
export function inheritedProfile(kind: string, source: SessionInfo | undefined): ProfileChoice | undefined {
  if (kind !== "claude" || source?.kind !== "claude") return undefined;
  return source.account_profile_id ?? "default";
}

export function parseProfileChoice(value: string): ProfileChoice | undefined {
  if (!value) return undefined;
  if (value === "default") return value;
  const id = Number(value);
  if (!Number.isSafeInteger(id) || id <= 0) throw new Error("Choose an available Claude profile");
  return id;
}
