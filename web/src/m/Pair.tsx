import { useState } from "preact/hooks";
import { normalizeCode } from "./logic.ts";
import { pair, pairBusy, pairError } from "./store.ts";

export function Pair() {
  const [code, setCode] = useState("");
  return (
    <main class="pair-screen">
      <img src="icon.svg" width="48" height="48" alt="" />
      <h1>
        Your agents,<br />within reach.
      </h1>
      <p>Pair this phone with Muster on your computer.</p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          pair(code);
        }}
      >
        <label for="pair-code">Pairing code</label>
        <input
          id="pair-code"
          class="pair-code"
          value={code}
          maxLength={16}
          autoCapitalize="characters"
          autoComplete="off"
          spellcheck={false}
          placeholder="ABCDEFGHJ"
          disabled={pairBusy.value}
          onInput={(e) => setCode(normalizeCode(e.currentTarget.value).slice(0, 9))}
          aria-describedby="pair-hint pair-error"
        />
        <button class="primary" type="submit" disabled={code.length !== 9 || pairBusy.value}>
          {pairBusy.value ? "Pairing…" : "Pair phone"}
        </button>
        <p id="pair-error" class="inline-error" role="alert">{pairError.value}</p>
      </form>
      <p id="pair-hint" class="pair-hint">
        Run <code>muster pair</code> on your computer and scan the QR code, or type the short code.
      </p>
      <footer>
        Muster <span>·</span> Connected over your tailnet
      </footer>
    </main>
  );
}
