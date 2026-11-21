// Dev-only: the mock daemon's pull request cards for the PRs to merge tab (docs/PROTOCOL.md "Pull
// requests to merge"), in memory. Nothing here calls GitHub: each card carries a made-up read, and
// `pr.merge` "merges" a ready card after a moment (any other is refused with its reason).
// `mock.pr_seed` puts in the sample cards (every state the tab shows), `mock.pr_seed_tall` two
// cards with a full-page phone picture (too tall to read: one posted after post_pr left such pictures
// out, one stored before); dev/mock.ts routes `pr.*` and `mock.pr_*` here. Every repo, name, commit and picture is invented: this repo is public.

import type { Pr, PrRead, PrVerdict } from "../src/types.ts";

export interface PrDeps {
  now: () => number;
  broadcast: (event: string, data: unknown) => void;
}

// ---------- invented screenshots of made-up apps ----------

const svg = (w: number, h: number, body: string) =>
  `<svg viewBox="0 0 ${w} ${h}" width="${w * 2}" height="${h * 2}" xmlns="http://www.w3.org/2000/svg" font-family="system-ui, sans-serif">${body}</svg>`;
const bars = (x: number, y: number, widths: number[], gap = 16, fill = "#c9cdd9") =>
  widths.map((w, i) => `<rect x="${x}" y="${y + i * gap}" width="${w}" height="7" rx="3.5" fill="${fill}"/>`).join("");
const notesFrame = (body: string) =>
  svg(640, 400, `<rect width="640" height="400" fill="#f7f8fb"/>
    <rect width="640" height="38" fill="#2c3654"/><circle cx="20" cy="19" r="7" fill="#f2b84b"/>
    <text x="36" y="24" fill="#fff" font-size="14" font-weight="600">Pocket Notes</text>
    <rect x="500" y="10" width="120" height="18" rx="9" fill="#414c70"/>
    <rect y="38" width="160" height="362" fill="#eceef5"/>
    <rect x="10" y="50" width="140" height="30" rx="6" fill="#dfe3f0"/><text x="22" y="70" font-size="12" fill="#2c3654" font-weight="600">Trip plan</text>
    <text x="22" y="104" font-size="12" fill="#5b6279">Reading list</text><text x="22" y="134" font-size="12" fill="#5b6279">Garden</text>
    <text x="22" y="164" font-size="12" fill="#5b6279">Recipes</text><text x="22" y="194" font-size="12" fill="#5b6279">Ideas</text>${body}`);

/** A made-up site on one phone screen (390 × 844), or the whole page (390 × 8000). */
const phone = (whole: boolean) => {
  const h = whole ? 8000 : 844;
  const blocks = Array.from({ length: Math.floor((h - 120) / 150) }, (_, i) => {
    const y = 110 + i * 150;
    return `<text x="20" y="${y}" font-size="17" font-weight="700" fill="#1e2a21">Section ${i + 1}</text>
      ${bars(20, y + 18, [330, 350, 300, 340, 220], 16, "#cfd3c8")}`;
  }).join("");
  return `<svg viewBox="0 0 390 ${h}" width="390" height="${h}" xmlns="http://www.w3.org/2000/svg" font-family="system-ui, sans-serif"><rect width="390" height="${h}" fill="#fbfaf7"/><rect width="390" height="56" fill="#fff"/>
    <rect y="56" width="390" height="1" fill="#e6e2d8"/><text x="20" y="36" font-size="18" font-weight="700" fill="#27412f">Fernleaf</text>
    <rect x="300" y="18" width="70" height="22" rx="11" fill="#27412f"/>${blocks}</svg>`;
};

const SHOTS: Record<string, (after: boolean) => string> = {
  phone: () => phone(false),
  phonepage: () => phone(true),
  editor: (after) =>
    notesFrame(`<text x="186" y="80" font-size="22" font-weight="700" fill="#1f2640">Trip plan</text>
    ${bars(186, 104, [410, 380, 424, 300])}${bars(186, 186, [396, 420, 250])}${bars(186, 252, [410, 340])}
    ${after ? `<rect x="186" y="340" width="430" height="1" fill="#d9dce8"/><rect x="186" y="354" width="92" height="24" rx="12" fill="#e3f3e8"/>
      <text x="199" y="371" font-size="12.5" font-weight="600" fill="#1f6b3a">128 words</text>` : ""}`),
  list: (after) =>
    notesFrame(["Trip plan", "Reading list", "Garden", "Recipes"].map((t, i) => {
      const y = 62 + i * 78, n = [128, 342, 57, 1204][i];
      return `<rect x="178" y="${y}" width="444" height="64" rx="8" fill="#fff" stroke="#e1e4ee"/>
      <text x="194" y="${y + 26}" font-size="14" font-weight="600" fill="#1f2640">${t}</text>${bars(194, y + 40, [250 - i * 30], 0)}
      ${after ? `<text x="606" y="${y + 26}" text-anchor="end" font-size="12" font-weight="600" fill="#1f6b3a">${n} words</text>` : ""}`;
    }).join("")),
  footer: (after) =>
    svg(360, 400, `<rect width="360" height="400" fill="#fbfaf7"/>
    <rect width="360" height="44" fill="#fff"/><rect y="44" width="360" height="1" fill="#e6e2d8"/>
    <text x="16" y="28" font-size="15" font-weight="700" fill="#27412f">Fernleaf</text>
    <rect x="16" y="66" width="328" height="120" rx="10" fill="#dfe9df"/>${bars(16, 206, [300, 328, 220], 16, "#cfd3c8")}
    <rect y="${after ? 318 : 290}" width="360" height="${after ? 82 : 110}" fill="#27412f"/>
    ${(after
      ? [["About", 16, 350], ["Pricing", 74, 350], ["Docs", 140, 350], ["Contact", 190, 350], ["Terms", 258, 350]]
      : [["About", 16, 322], ["Pricing", 90, 322], ["Docs", 172, 322], ["Contact", 236, 322], ["Terms", 16, 350]])
      .map(([t, x, y]) => `<text x="${x}" y="${y}" font-size="${after ? 13 : 15}" fill="#e8f0e6">${t}</text>`).join("")}
    <text x="16" y="386" font-size="11" fill="#9db3a1">Made-up sample site</text>`),
  term: (after) =>
    svg(640, 400, `<rect width="640" height="400" fill="#16181d"/>
    <g font-family="ui-monospace, monospace" font-size="13" fill="#c9ced8">
    <text x="20" y="36"><tspan fill="#7fd48a">$</tspan> billing export --month 2026-10</text>
    ${after
      ? `<text x="20" y="64">Wrote 3 files:</text><text x="20" y="88">  invoices-2026-10.pdf</text>
         <text x="20" y="112" fill="#e8c170">  invoices-2026-10.csv   42 rows</text><text x="20" y="136">  summary.txt</text>`
      : `<text x="20" y="64">Wrote 2 files:</text><text x="20" y="88">  invoices-2026-10.pdf</text><text x="20" y="112">  summary.txt</text>`}
    <text x="20" y="${after ? 172 : 148}"><tspan fill="#7fd48a">$</tspan> <tspan fill="#8b93a3">▍</tspan></text></g>`),
  tags: (after) =>
    notesFrame(`<text x="186" y="80" font-size="18" font-weight="700" fill="#1f2640">Tags</text>` +
      (after ? [["travel", 31], ["books", 18], ["garden", 9], ["food", 4]] : [["books", 18], ["food", 4], ["garden", 9], ["travel", 31]])
        .map(([t, n], i) => `<rect x="186" y="${98 + i * 44}" width="430" height="34" rx="6" fill="#fff" stroke="#e1e4ee"/>
      <text x="200" y="${120 + i * 44}" font-size="13" fill="#1f2640">${t}</text>
      <text x="602" y="${120 + i * 44}" text-anchor="end" font-size="12" fill="#5b6279">${n} notes</text>`).join("")),
  sitemap: () =>
    svg(640, 400, `<rect width="640" height="400" fill="#fbfaf7"/><rect width="640" height="48" fill="#fff"/>
    <rect y="48" width="640" height="1" fill="#e6e2d8"/><text x="24" y="31" font-size="16" font-weight="700" fill="#27412f">Fernleaf</text>
    <text x="24" y="96" font-size="24" font-weight="700" fill="#1e2a21">Sitemap</text>
    ${["Home", "Pricing", "Docs", "Contact"].map((t, i) => `<text x="${24 + (i % 2) * 300}" y="${140 + Math.floor(i / 2) * 110}" font-size="15" font-weight="600" fill="#27412f">${t}</text>
      ${bars(24 + (i % 2) * 300, 156 + Math.floor(i / 2) * 110, [180, 140, 160], 16, "#cfd3c8")}`).join("")}`),
  docs: (after) =>
    svg(640, 400, `<rect width="640" height="400" fill="#fff"/><text x="32" y="56" font-size="22" font-weight="700" fill="#1d2433">Install on Linux</text>
    ${bars(32, 80, [520, 440])}<rect x="32" y="124" width="576" height="${after ? 96 : 60}" rx="8" fill="#f1f3f8"/>
    <g font-family="ui-monospace, monospace" font-size="13" fill="#2b3350"><text x="48" y="150">curl -LO example.test/nw.tar.gz</text>
    ${after ? `<text x="48" y="174">tar -xzf nw.tar.gz</text><text x="48" y="198">./nw/install.sh</text>` : `<text x="48" y="170">./install.sh</text>`}</g>
    ${bars(32, after ? 248 : 212, [500, 380, 460])}`),
};

// ---------- invented pull requests ----------

/** A made-up 40-character commit id from a short one. */
const sha = (short: string) => (short + "0c41d9e77b2a5f08c3d6e19a4b7f2c5d8e0a1b3").slice(0, 40);

type Kind = "ready" | "nochecks" | "running" | "stale" | "refused" | "later" | "merged" | "closed";

interface Sample {
  n: number;
  repo: string;
  title: string;
  agent: string;
  age_min: number;
  sha: string;
  files: number;
  add: number;
  del: number;
  kind: Kind;
  checks: [number, number, number, number]; // total, passed, failed, running
  summary: string;
  who: string;
  tests: string;
  undo: string;
  after: string;
  pics: ["before" | "after", string, string][];
  /** Posted after post_pr left out too-tall pictures: these are marked, and kept no copy of. */
  leftOut?: { index: number; width: number; height: number }[];
  base?: string;
  newSha?: string;
  merged_as?: string;
}

const SAMPLES: Sample[] = [
  {
    n: 214, repo: "acme/notes-app", title: "Show a word count under each note", agent: "builder-2", age_min: 25, sha: "9f3c2ab",
    files: 4, add: 86, del: 12, kind: "ready", checks: [3, 3, 0, 0],
    summary: "Each note now shows its word count under the text, and the notes list shows the count on every row.",
    who: "Everyone who writes notes. The count updates as they type.",
    tests: "deno test: 212 passed, 0 failed. Opened the editor and the list in the sample app at 1100 and 480 px wide.",
    undo: "Revert the merge commit. Stored notes are untouched.", after: "Reload the window.",
    pics: [["before", "The editor has no count", "editor"], ["after", "“128 words” sits under the text", "editor"],
      ["before", "The notes list", "list"], ["after", "Every row shows its count", "list"]],
  },
  {
    n: 58, repo: "acme/site-kit", title: "Keep the footer links on one row on phones", agent: "web-fixer", age_min: 12, sha: "4be71d0",
    files: 2, add: 14, del: 9, kind: "nochecks", checks: [0, 0, 0, 0],
    summary: "On phones the five footer links now fit on one row; “Terms” used to drop to a second row.",
    who: "Visitors on screens narrower than 400 px.",
    tests: "deno task test: 48 passed, 0 failed. Looked at the footer at 360 and 480 px wide.",
    undo: "Revert the merge commit.", after: "Deploy the site (the usual deploy command).",
    pics: [["before", "360 px wide: “Terms” wraps to a second row", "footer"], ["after", "360 px wide: all five on one row", "footer"]],
  },
  {
    n: 77, repo: "northwind/docs", title: "Fix the install steps for Linux", agent: "docs-writer", age_min: 7, sha: "c02e9a4",
    files: 1, add: 6, del: 2, kind: "running", checks: [2, 1, 0, 1],
    summary: "The Linux install page now unpacks the download before running the installer.",
    who: "Anyone installing on Linux from the docs.", tests: "Ran the three commands in a clean container.",
    undo: "Revert the merge commit.", after: "The docs site rebuilds by itself.",
    pics: [["before", "The unpack step is missing", "docs"], ["after", "Three commands, in order", "docs"]],
  },
  {
    n: 31, repo: "northwind/billing-cli", title: "Export invoices as CSV", agent: "builder-1", age_min: 77, sha: "77d0c1e",
    files: 6, add: 203, del: 18, kind: "stale", checks: [2, 2, 0, 0], newSha: "e51a9b2",
    summary: "The export command also writes a CSV file with one row per invoice.",
    who: "Anyone who runs the monthly export.", tests: "cargo test -q: 96 passed. Ran the export on the sample month.",
    undo: "Revert the merge commit.", after: "Build and install the new version.",
    pics: [["before", "Export writes a PDF and a summary", "term"], ["after", "Export also writes a CSV", "term"]],
  },
  {
    n: 209, repo: "acme/notes-app", title: "Sort tags by most used", agent: "builder-3", age_min: 107, sha: "0a6d5f3",
    files: 3, add: 41, del: 22, kind: "refused", checks: [3, 3, 0, 0],
    summary: "The Tags page lists the most used tag first; it was alphabetical.",
    who: "Everyone who opens the Tags page.", tests: "deno test: 209 passed, 0 failed.",
    undo: "Revert the merge commit.", after: "Reload the window.",
    pics: [["before", "Tags in alphabetical order", "tags"], ["after", "Most used first", "tags"]],
  },
  {
    n: 201, repo: "acme/notes-app", title: "Try a darker sidebar", agent: "builder-2", age_min: 1500, sha: "b8e2f70",
    files: 1, add: 4, del: 4, kind: "later", checks: [3, 3, 0, 0], summary: "The sidebar is two shades darker, so the open note stands out more.",
    who: "Everyone, on every page.", tests: "deno test: 208 passed.", undo: "Revert the merge commit.", after: "Reload the window.", pics: [],
  },
  {
    n: 57, repo: "acme/site-kit", title: "Add a sitemap page", agent: "web-fixer", age_min: 56, sha: "5c9e0d2",
    files: 3, add: 72, del: 0, kind: "merged", checks: [0, 0, 0, 0], merged_as: "abc1234",
    summary: "A new Sitemap page lists every page of the site, linked from the footer.",
    who: "Visitors who look for a page, and search engines.", tests: "deno task test: 47 passed, 0 failed.",
    undo: "Revert the merge commit.", after: "Deploy the site (the usual deploy command).",
    pics: [["after", "The new Sitemap page", "sitemap"]],
  },
  {
    n: 199, repo: "acme/site-kit", title: "Try a serif font for headings", agent: "web-fixer", age_min: 300, sha: "2d7b4e1",
    files: 2, add: 8, del: 8, kind: "closed", checks: [0, 0, 0, 0],
    summary: "Headings use a serif font; body text is unchanged.", who: "Every visitor, on every page.",
    tests: "deno task test: 47 passed. Looked at the home and pricing pages.", undo: "Revert the merge commit.",
    after: "Deploy the site.", pics: [],
  },
];

/** Pull requests with a full-page phone picture: #231 was posted after post_pr left such pictures
 * out (its card marks it), #232 before (its stored copy is measured when the window asks). */
const TALL_SAMPLES: Sample[] = [231, 232].map((n): Sample => ({
  n, repo: "acme/site-kit", title: n === 231 ? "Shorter sign-up page on phones" : "Sign-up page in two steps", agent: "web-fixer",
  age_min: n === 231 ? 12 : 30, sha: n === 231 ? "5e1d0aa" : "7c2b9f4", files: 3, add: 40, del: 61, kind: "ready", checks: [2, 2, 0, 0],
  summary: "The sign-up page on phones drops the long feature list, so the form is on the first screen.",
  who: "Visitors who sign up from a phone.", tests: "deno test: 51 passed. Looked at the page at 390 and 1280 px wide.",
  undo: "Revert the merge commit.", after: "Deploy the site.",
  pics: [["before", "One phone screen: the form is below the fold", "phone"], ["after", "The whole page on a phone", "phonepage"],
    ["before", "On a laptop, before", "docs"], ["after", "On a laptop, after", "docs"]],
  ...(n === 231 ? { leftOut: [{ index: 1, width: 390, height: 8000 }] } : {}),
}));

/** The width and height attributes of an `<svg>` (the mock's pictures all have them). */
function svgSize(body: string): { width: number; height: number } | null {
  const tag = body.slice(0, body.indexOf(">"));
  const w = Number(/\swidth="(\d+)"/.exec(tag)?.[1]), h = Number(/\sheight="(\d+)"/.exec(tag)?.[1]);
  return w > 0 && h > 0 ? { width: w, height: h } : null;
}

export function prMock(d: PrDeps) {
  const all = new Map<number, Pr>();
  /** Picture SVGs by `<id>:<index>`. */
  const svgs = new Map<string, string>();
  let next = 1;

  const publish = (p: Pr) => d.broadcast("pr_upserted", p);
  const get = (id: number) => {
    const p = all.get(Number(id));
    if (!p) throw new Error(`no pull request card P${id}`);
    return p;
  };
  const touch = (p: Pr) => { p.updated_at = Math.max(d.now(), p.updated_at + 1); };

  function readOf(s: Sample, at: number): PrRead {
    const [total, passed, failed, running] = s.checks;
    return {
      at, state: s.kind === "merged" ? "MERGED" : s.kind === "closed" ? "CLOSED" : "OPEN", draft: false, head_sha: sha(s.kind === "stale" ? s.newSha! : s.sha),
      base: s.base ?? (s.repo.startsWith("northwind") ? "main" : "master"),
      mergeable: s.kind === "refused" ? "CONFLICTING" : s.kind === "merged" ? "UNKNOWN" : "MERGEABLE", merge_state: "CLEAN",
      title: s.title, url: `https://github.com/${s.repo}/pull/${s.n}`, author: s.agent, additions: s.add, deletions: s.del,
      files: s.files, ...(s.merged_as ? { merge_commit: sha(s.merged_as) } : {}),
      checks: { total, passed, failed, running, ...(running ? { waiting: ["integration"] } : {}) },
    };
  }

  function verdictOf(r: PrRead, posted: string): PrVerdict {
    const state = r.state === "OPEN" && !r.draft ? "pass" : "fail";
    const commit = r.head_sha === posted ? "pass" : "warn";
    const base = r.mergeable === "MERGEABLE" ? "pass" : r.mergeable === "CONFLICTING" ? "fail" : "wait";
    const c = r.checks;
    const checks = c.failed ? "fail" : c.running ? "wait" : c.total === 0 ? "none" : "pass";
    const reason = state === "fail" ? (r.state === "MERGED" ? "it is merged on GitHub already" : "it is closed on GitHub")
      : commit !== "pass" ? "it has new commits since it was posted"
      : base === "fail" ? `it conflicts with ${r.base}`
      : checks === "fail" ? `${c.failed} of ${c.total} checks failed`
      : checks === "wait" ? "a check is still running" : undefined;
    return { ok: !reason, state, commit, base, checks, ...(reason ? { reason } : {}) };
  }

  function seed(samples = SAMPLES): Pr[] {
    const out: Pr[] = [];
    for (const s of samples) {
      const id = next++;
      const at = d.now() - s.age_min * 60_000;
      const read = readOf(s, d.now() - 2 * 60_000);
      const p: Pr = {
        id, card: `P${id}`, status: s.kind === "merged" ? "merged" : s.kind === "stale" ? "stale" : s.kind === "closed" ? "closed" : "waiting",
        created_at: at, updated_at: at + 1, poster: { label: s.agent, session_id: 0, kind: "codex", agent_id: 2000 + id },
        repo: s.repo, number: s.n, head_sha: sha(s.sha), summary: s.summary, who_notices: s.who,
        pictures: s.pics.map(([side, caption], i) => {
          const tall = s.leftOut?.find((x) => x.index === i);
          return { path: `/home/user/shots/${s.n}-${side}-${i + 1}.svg`, caption, side, file: tall ? "" : `1/${i}.svg`,
            ...(tall ? { too_tall: { width: tall.width, height: tall.height } } : {}) };
        }),
        tests: s.tests, undo: s.undo, after_merge: s.after, post: 1, github: read, verdict: verdictOf(read, sha(s.sha)),
        ...(s.kind === "later" ? { later: d.now() - 3600_000 } : {}),
        ...(s.kind === "stale" ? { stale_told: true } : {}),
        ...(s.kind === "refused"
          ? { refused: { at: d.now() - 6 * 60_000, reason: `it conflicts with ${read.base}`,
            github_said: "Pull request is not mergeable: the merge commit cannot be cleanly created." },
            merge: { at: d.now() - 6 * 60_000, sha: sha(s.sha), done_at: d.now() - 6 * 60_000 } }
          : {}),
        ...(s.kind === "merged"
          ? {
            merge: { at: d.now() - 40 * 60_000, sha: sha(s.sha), sent_at: d.now() - 40 * 60_000, done_at: d.now() - 39 * 60_000,
              commit: sha(s.merged_as!) },
            outbox: [{ kind: "merged" as const, at: d.now() - 39 * 60_000, from: "muster",
              delivery: { status: "delivered" as const, to: s.agent, note: `typed into ${s.agent}` } }],
          }
          : {}),
        ...(s.kind === "closed" ? { closed: { at: d.now() - 20 * 60_000, reason: "closed on GitHub" } } : {}),
        details: s.kind === "ready" ? "Files: src/editor/count.ts, src/list/row.tsx. Reviewed twice; both found nothing." : undefined,
      };
      if (!p.details) delete p.details;
      s.pics.forEach(([side, , shot], i) => !s.leftOut?.some((x) => x.index === i) && svgs.set(`${id}:${i}`, SHOTS[shot](side === "after")));
      all.set(id, p);
      publish(p);
      out.push(p);
    }
    return out;
  }

  function finish(c: Pr) {
    if (c.verdict?.ok && !c.refused) {
      c.status = "merged";
      c.merge = { ...c.merge!, sent_at: d.now(), done_at: d.now(), commit: sha("abc1234") };
      if (c.github) c.github = { ...c.github, state: "MERGED", merge_commit: sha("abc1234") };
      c.outbox = [...(c.outbox ?? []), { kind: "merged", at: d.now(), from: "muster",
        delivery: { status: "delivered", to: c.poster.label, note: `typed into ${c.poster.label}` } }];
    } else {
      c.status = "waiting";
      c.refused = { at: d.now(), reason: c.verdict?.reason ?? "GitHub refused the merge" };
      c.merge = { ...c.merge!, done_at: d.now() };
    }
    touch(c);
    publish(c);
  }

  const listed = () => [...all.values()].filter((p) => p.status !== "withdrawn" && p.status !== "changes_asked");

  return {
    seed,
    handle(method: string, p: any): unknown {
      switch (method) {
        case "pr.list":
          return { prs: listed().sort((a, b) => a.id - b.id) };
        case "pr.get":
          return get(p.id);
        case "pr.picture": {
          const card = get(p.id);
          const pic = card.pictures?.[p.index];
          // Too tall to read: only its size, as the daemon sends it (left out, or measured).
          if (pic?.too_tall) return { too_tall: pic.too_tall, caption: pic.caption };
          const body = svgs.get(`${card.id}:${p.index}`);
          if (!pic || !body) throw new Error(`card P${card.id} has no picture ${Number(p.index) + 1}`);
          const size = svgSize(body);
          if (size && size.height > 3 * size.width) return { too_tall: size, caption: pic.caption };
          return { src: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(body)}`, caption: pic.caption };
        }
        case "pr.refresh": {
          // The made-up reads don't change: only their time moves on.
          const cards = p.id != null ? [get(p.id)] : listed().filter((c) => c.status === "waiting" || c.status === "stale");
          for (const c of cards) {
            if (!c.github || (c.status !== "waiting" && c.status !== "stale")) continue;
            c.github = { ...c.github, at: d.now() };
            touch(c);
            publish(c);
          }
          return p.id != null ? get(p.id) : { prs: listed() };
        }
        case "pr.merge": {
          const c = get(p.id);
          if (c.status !== "waiting") throw new Error(`${c.repo} #${c.number} is ${c.status}`);
          if (c.head_sha !== String(p.sha).toLowerCase()) throw new Error(`the card for ${c.repo} #${c.number} changed since you looked`);
          if (c.github?.base !== p.base || c.post !== p.post) throw new Error(`the card for ${c.repo} #${c.number} changed since you looked`);
          c.status = "merging";
          c.merge = { at: d.now(), sha: c.head_sha, post: c.post, base: String(p.base) };
          touch(c);
          publish(c);
          // Answered at once, merging; the result comes as an event (as the daemon does).
          setTimeout(() => finish(c), 1200);
          return { ...c };
        }
        case "pr.ask_changes": {
          const c = get(p.id);
          if (!String(p.words ?? "").trim()) throw new Error("say what to change (1 to 4000 characters)");
          c.status = "changes_asked";
          c.changes = { at: d.now(), words: String(p.words).trim() };
          touch(c);
          publish(c);
          return c;
        }
        case "pr.later": {
          const c = get(p.id);
          if (p.later) c.later = d.now();
          else delete c.later;
          touch(c);
          publish(c);
          return c;
        }
        case "mock.pr_seed":
          return { prs: seed() };
        case "mock.pr_seed_tall":
          return { prs: seed(TALL_SAMPLES) };
      }
      throw new Error(`unknown method: ${method}`);
    },
  };
}
