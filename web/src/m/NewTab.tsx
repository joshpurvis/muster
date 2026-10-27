// "New tab" for a manage-scoped phone: a folder, what to start in it, an optional first prompt.
import { useEffect, useRef, useState } from "preact/hooks";
import { rpc } from "../rpc.ts";
import type { SessionKind } from "../types.ts";
import {
  canManage,
  hideNewTab,
  info,
  loadInfo,
  newTabOpen,
  openTab,
  showNewTab,
  showStarted,
} from "./manage.ts";
import { KIND_LABEL, tildify, workspaceChoices } from "./manage_logic.ts";
import { errorText, ready, sessions } from "./store.ts";
import { Icon } from "./ui.tsx";
import "./manage.css";

const KINDS: SessionKind[] = ["claude", "codex", "shell"];
const OTHER = -1;

/** The topbar's "+" (manage only). */
export function NewTabButton() {
  if (!canManage.value) return null;
  return (
    <button type="button" class="icon-button" aria-label="New tab" onClick={showNewTab}>
      <Icon name="plus" />
    </button>
  );
}

/** The screen it opens, over the sessions list. */
export function NewTabLayer() {
  return canManage.value && newTabOpen.value ? <NewTabScreen /> : null;
}

function NewTabScreen() {
  const [choice, setChoice] = useState<number | null>(null);
  const [path, setPath] = useState("");
  const [agent, setAgent] = useState<SessionKind>("claude");
  const [worktree, setWorktree] = useState(false);
  const [prompt, setPrompt] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [loadError, setLoadError] = useState("");
  const data = info.value;
  const online = ready.value;
  // Fresh folders each time the screen opens, and again after a reconnect.
  useEffect(() => {
    if (online) loadInfo().then(() => setLoadError(""), (e) => setLoadError(errorText(e)));
  }, [online]);
  const choices = data
    ? workspaceChoices(data.workspaces, data.channels, [...sessions.value.values()], data.agents)
    : [];
  // Until the user picks, the most recently used folder (or a typed one when there is none).
  const picked = choice ?? (data ? choices[0]?.id ?? OTHER : null);
  const ws = choices.find((w) => w.id === picked);
  const notGit = !!ws && !ws.is_git;
  const canOpen = ready.value && !busy && (ws ? true : picked === OTHER && !!path.trim());
  const submit = async (e: Event) => {
    e.preventDefault();
    if (!canOpen) return;
    setBusy(true);
    setError("");
    try {
      const s = await openTab({
        workspace_id: ws?.id,
        path: ws ? undefined : path,
        agent,
        worktree: worktree && !notGit,
        prompt,
      });
      showStarted(s);
    } catch (err) {
      setError(errorText(err));
      setBusy(false);
    }
  };
  return (
    <main class="new-tab-screen" aria-labelledby="new-tab-title">
      <header class="topbar session-header">
        <button type="button" class="icon-button" aria-label="Back to sessions" onClick={hideNewTab}>
          <Icon name="back" />
        </button>
        <h1 id="new-tab-title">New tab</h1>
      </header>
      <form class="new-tab-form" onSubmit={submit}>
        <div class="new-tab-body">
          <fieldset class="field">
            <legend>Folder</legend>
            {!data && !loadError && <p class="field-hint">Loading folders…</p>}
            {loadError && <p class="inline-error" role="alert">{loadError}</p>}
            <div class="choice-list" role="radiogroup" aria-label="Folder">
              {choices.slice(0, 8).map((w) => (
                <button
                  type="button"
                  role="radio"
                  key={w.id}
                  aria-checked={picked === w.id}
                  class="choice"
                  onClick={() => setChoice(w.id)}
                >
                  <span class="choice-text">
                    <strong>{w.name}</strong>
                    <span>{tildify(w.path, data?.home ?? "")}</span>
                  </span>
                  <span class="choice-mark" aria-hidden="true" />
                </button>
              ))}
              {data && (
                <button
                  type="button"
                  role="radio"
                  aria-checked={picked === OTHER}
                  class="choice"
                  onClick={() => setChoice(OTHER)}
                >
                  <span class="choice-text">
                    <strong>Another folder</strong>
                    <span>Type a path on your computer</span>
                  </span>
                  <span class="choice-mark" aria-hidden="true" />
                </button>
              )}
            </div>
            {picked === OTHER && <FolderField value={path} onChange={setPath} />}
          </fieldset>
          <fieldset class="field">
            <legend>Start with</legend>
            <div class="segmented" role="radiogroup" aria-label="Start with">
              {KINDS.map((k) => (
                <button
                  type="button"
                  role="radio"
                  key={k}
                  aria-checked={agent === k}
                  onClick={() => setAgent(k)}
                >
                  {KIND_LABEL[k]}
                </button>
              ))}
            </div>
          </fieldset>
          <label class={`check-row ${notGit ? "disabled" : ""}`}>
            <input
              type="checkbox"
              checked={worktree && !notGit}
              disabled={notGit}
              onChange={(e) => setWorktree(e.currentTarget.checked)}
            />
            <span>
              Own git worktree
              <small>
                {notGit ? "This folder isn't a git repository." : "An isolated checkout on a new branch."}
              </small>
            </span>
          </label>
          {agent !== "shell" && (
            <label class="field">
              <span class="field-label">
                First prompt <span class="optional">optional</span>
              </span>
              <textarea
                rows={4}
                value={prompt}
                placeholder="What should the agent do?"
                autoCapitalize="sentences"
                onInput={(e) => setPrompt(e.currentTarget.value)}
              />
            </label>
          )}
          <p class="field-hint">
            The tab names itself and uses the default permissions. Change permissions on your computer.
          </p>
          {error && <p class="inline-error" role="alert">{error}</p>}
        </div>
        <footer class="new-tab-foot">
          <button type="submit" class="primary" disabled={!canOpen}>
            {busy ? "Opening…" : `Open ${KIND_LABEL[agent]} tab`}
          </button>
        </footer>
      </form>
    </main>
  );
}

/** A path box with `fs.complete` suggestions as rows to tap (one level at a time). */
function FolderField({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const [entries, setEntries] = useState<string[]>([]);
  const seq = useRef(0);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => input.current?.focus(), []);
  useEffect(() => {
    const my = ++seq.current;
    const timer = setTimeout(() => {
      rpc.call<{ entries: string[] }>("fs.complete", { prefix: value || "~/" })
        .then((r) => my === seq.current && setEntries(r.entries.slice(0, 30)))
        .catch(() => my === seq.current && setEntries([]));
    }, 120);
    return () => clearTimeout(timer);
  }, [value]);
  const shown = entries.filter((e) => e.replace(/\/$/, "") !== value.replace(/\/$/, ""));
  return (
    <div class="folder-field">
      <input
        ref={input}
        value={value}
        placeholder="~/projects/my-repo"
        aria-label="Folder path"
        autoCapitalize="off"
        autoCorrect="off"
        autoComplete="off"
        spellcheck={false}
        enterKeyHint="done"
        onInput={(e) => onChange(e.currentTarget.value)}
      />
      {shown.length > 0 && (
        <ul class="folder-suggestions" aria-label="Folders">
          {shown.map((entry) => (
            <li key={entry}>
              <button
                type="button"
                onPointerDown={(e) => e.preventDefault()}
                onClick={() => {
                  onChange(entry.endsWith("/") ? entry : entry + "/");
                  input.current?.focus();
                }}
              >
                {entry}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
