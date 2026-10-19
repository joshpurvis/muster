// Assertions for the panel app tests (the web package has no test dependencies).
export function check(value: unknown, message = "assertion failed"): asserts value {
  if (!value) throw new Error(message);
}

export function eq(actual: unknown, expected: unknown, message = "") {
  const a = JSON.stringify(actual), e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${message ? message + ": " : ""}expected ${e}, got ${a}`);
}

/** The errors of a failed validation, or a thrown error if it passed. */
export function errorsOf(r: { ok: boolean; errors?: string[] }): string[] {
  if (r.ok) throw new Error("expected the input to be rejected");
  return r.errors!;
}
