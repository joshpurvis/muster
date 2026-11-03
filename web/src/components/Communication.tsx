import { useEffect, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import { workspaces } from "../state.ts";

type Grant = {
  from_workspace: number;
  to_workspace: number;
  from_session: number | null;
  message: boolean;
  read_output: boolean;
};
type Message = {
  status: string;
  id: number;
  from: number;
  to: number;
  text: string;
  reply: string | null;
  error: string | null;
  timed_out: boolean;
  delivered_at: number | null;
};
type Snapshot = { grants: Grant[]; messages: Message[] };
export function Communication() {
  const [open, setOpen] = useState(false);
  const [data, setData] = useState<Snapshot>({ grants: [], messages: [] });
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [session, setSession] = useState("");
  const [message, setMessage] = useState(true);
  const [read, setRead] = useState(false);
  async function refresh() {
    try {
      setData(await rpc.call<Snapshot>("communication.get", {}));
      setError("");
    } catch (e) {
      setError(String(e));
    }
  }
  useEffect(() => {
    if (open) {
      void refresh();
      const timer = setInterval(() => void refresh(), 3000);
      return () => clearInterval(timer);
    }
  }, [open]);
  async function save(grants: Grant[]) {
    setBusy(true);
    try {
      setData(await rpc.call<Snapshot>("communication.set", { grants, expected_grants: data.grants }));
      setError("");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div class="communication">
      <button type="button" aria-expanded={open} onClick={() => setOpen(!open)}>Session communication</button>
      {open && (
        <section aria-label="Session communication">
          <p>
            Sessions in the same workspace can communicate. Add directional sharing rules for other
            workspaces. Named agents remain shared services.
          </p>
          {error && <p role="alert">{error}</p>}
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void save([...data.grants, {
                from_workspace: Number(from),
                to_workspace: Number(to),
                from_session: session ? Number(session) : null,
                message,
                read_output: read,
              }]);
            }}
          >
            <label>
              From workspace<select required value={from} onChange={(e) => setFrom(e.currentTarget.value)}>
                <option value="">Choose workspace</option>
                {workspaces.value.map((w) => <option key={w.id} value={w.id}>{w.name}</option>)}
              </select>
            </label>
            <label>
              To workspace<select required value={to} onChange={(e) => setTo(e.currentTarget.value)}>
                <option value="">Choose workspace</option>
                {workspaces.value.map((w) => <option key={w.id} value={w.id}>{w.name}</option>)}
              </select>
            </label>
            <label>
              Caller session ID (optional)<input
                type="number"
                min="1"
                step="1"
                value={session}
                onInput={(e) => setSession(e.currentTarget.value)}
              />
            </label>
            <label>
              <input
                type="checkbox"
                checked={message}
                onChange={(e) => setMessage(e.currentTarget.checked)}
              />Send messages
            </label>
            <label>
              <input type="checkbox" checked={read} onChange={(e) => setRead(e.currentTarget.checked)} />Read
              output and channel updates
            </label>
            <button type="submit" disabled={busy || (!message && !read) || from === to}>
              Add sharing rule
            </button>
          </form>
          {data.grants.map((g, i) => (
            <div key={`${g.from_workspace}:${g.to_workspace}:${i}`}>
              <p>
                {workspaces.value.find((w) => w.id === g.from_workspace)?.name ?? g.from_workspace} →{" "}
                {workspaces.value.find((w) => w.id === g.to_workspace)?.name ?? g.to_workspace}
                {g.from_session ? ` (session ${g.from_session})` : ""}:{" "}
                {[g.message && "message", g.read_output && "read"].filter(Boolean).join(", ")}
              </p>
              <button
                type="button"
                disabled={busy}
                onClick={() => void save(data.grants.filter((_, n) => n !== i))}
              >
                Remove rule
              </button>
            </div>
          ))}
          <h3>Conversations</h3>
          {!data.messages.length && <p>No session requests yet.</p>}
          {data.messages.slice().reverse().map((m) => (
            <details key={m.id}>
              <summary>#{m.id} · {m.from} → {m.to} · {m.status.replaceAll("_", " ")}</summary>
              <p style={{ whiteSpace: "pre-wrap" }}>{m.text}</p>
              {m.reply !== null && <p style={{ whiteSpace: "pre-wrap" }}>Reply: {m.reply}</p>}
              {m.error && <p>{m.error}</p>}
            </details>
          ))}
          <p>Channel subscriptions collect posts for agents to read; they do not start new turns.</p>
        </section>
      )}
    </div>
  );
}
