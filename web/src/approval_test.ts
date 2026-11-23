import { approvalParams, type Decision } from "./types.ts";

Deno.test("window approvals include the sequence and timestamp of the request", () => {
  const s = { id: 7, state_seq: 19, state_since: 400 };
  for (const decision of ["approve", "approve_always", "deny"] as Decision[]) {
    const sent = approvalParams(s, decision);
    for (const [key, value] of Object.entries({ ...s, decision })) {
      if (Reflect.get(sent, key) !== value) throw new Error(`Missing or incorrect ${key}`);
    }
  }
});
