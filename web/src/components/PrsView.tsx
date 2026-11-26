// The PRs to merge tab (docs/UI.md "PRs to merge"): one pull request an agent wants merged fills
// the tab, pictures first; a strip of pills switches between them. The dock at the bottom shows
// what Muster itself read from GitHub (the four rules) and Merge. A click on Merge starts that
// card's 5 second countdown with Cancel in Merge's exact place; only then does the window send
// `pr.merge`, and Muster's own code (never a model) reads the pull request again and merges that
// exact commit. The countdown runs in the background (prs_merges.ts): moving to another card,
// clicking its Merge, opening a picture or the list, or leaving the tab never stops it; only that
// card's Cancel (or Esc while it is shown) does, or the card changing. Several cards can count
// down and merge at once. A merge's result never moves focus, the card shown or the list: the
// card just says how it went, and a card merged from here keeps its place in the Waiting strip.
//
// Why a stray key or click can't merge: no shortcut for Merge anywhere; plain Enter and Space on
// it do nothing (Ctrl+Enter does); clicks are ignored for 600 ms after the card changes, a Cancel,
// a reorder or the large view or list closing (the dock says so when it ignores one); the
// countdown keeps the dock's layout, so a double click lands its second click on Cancel; Merge
// and "Next waiting" are at opposite ends of the dock.
//
// A new card (posted, or posted again, while the tab is open: `arrivedPrs`) takes the stage when
// the tab shows nothing or a card that is done with (merged, closed, withdrawn), unless the pointer
// or the keyboard is on the dock, the list or a large picture is open. Otherwise the bar says it
// came ("1 new: <title>", a button that shows it), and the strip scrolls to its pill, marked until
// it is looked at. Off screen, prs_notice.ts posts a toast instead.
import { signal } from "@preact/signals";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import {
  arrivedPrs, askChanges, banner, baseName, canMerge, cardState, CLICK_GUARD_MS, COUNTDOWN_S, GROUPS, groupOf, leftOut, leftOutText, loadPrPicture,
  makesWay, mergeCommand, mergedPrs, nb, nextReady, offReason, otherSide, pairs, pairText, pictureKey, pillMark, prPictures,
  prs, prsFilter, prsInView, seenPr, refreshPrs, REFRESH_EVERY_MS, repoName, type Rule, rules, selectedPr, setLater, shortSha, sizeOf,
  sortWaiting, stepPicture, tagText, target, titleOf, toldText, waitingPrs,
} from "../prs.ts";
import { cancelMerge, clearSettled, failedMerges, mergedHere, queuedMerges, startMerge } from "../prs_merges.ts";
import { ageText, clockText } from "../questions_model.ts";
import type { Pr } from "../types.ts";
import { agents, sessions, toast } from "../state.ts";
import * as A from "../actions.ts";
import { cx } from "../util.ts";
import { LinkedText } from "./QuestionsView.tsx";
import { PRS_TAB } from "../layout/tree.ts";

const NARROW_PX = 760;
/** Below this height the dock stops sticking to the bottom (it would leave no room for the card). */
const SHORT_PX = 620;

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Bumped when the large view or the list closes: Merge ignores clicks for a moment after. */
const guardBump = signal(0);

function useNow(ms = 30_000): number {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

const IconMerge = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"
    aria-hidden="true">
    <circle cx="6" cy="5" r="2.5" /><circle cx="6" cy="19" r="2.5" /><circle cx="18" cy="12" r="2.5" />
    <path d="M6 7.5v9M6 8c0 4 4 4 9.5 4" />
  </svg>
);

/** The poster's session as this window has it now: a named agent's current one, else the one that posted. */
function posterSession(p: Pr) {
  const agent = p.poster.agent_id != null ? agents.value.find((a) => a.id === p.poster.agent_id) : undefined;
  const sid = agent?.session_id ?? p.poster.session_id;
  return sid != null ? sessions.value.get(sid) : undefined;
}

/** Focus the shown card's title (after picking a card or a Cancel; never on a merge's result). */
function focusTitle() {
  requestAnimationFrame(() => document.querySelector<HTMLElement>(".prs-view .pb-stage .qv-q")?.focus({ preventScroll: true }));
}

export function PrsView() {
  const ref = useRef<HTMLDivElement>(null);
  const [narrow, setNarrow] = useState(false);
  const [short, setShort] = useState(false);
  const [menu, setMenu] = useState(false);
  const [zoom, setZoom] = useState<{ id: number; post: number; index: number } | null>(null);
  const now = useNow();

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const size = () => {
      setNarrow(el.clientWidth < NARROW_PX);
      setShort(el.clientHeight < SHORT_PX);
    };
    const ro = new ResizeObserver(size);
    ro.observe(el);
    size();
    return () => ro.disconnect();
  }, []);

  // A new visit to the tab starts the Waiting strip afresh (cards merged meanwhile leave it).
  useEffect(() => clearSettled(), []);

  // On screen or not (a group zoomed or full screen over this one hides it): off screen, a new
  // card gets a toast instead (prs_notice.ts).
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    // Known at once (a hidden group is display: none, so it has no boxes); then as it changes.
    prsInView.value = el.getClientRects().length > 0;
    const io = new IntersectionObserver((es) => { prsInView.value = es.some((e) => e.isIntersecting); });
    io.observe(el);
    return () => {
      io.disconnect();
      prsInView.value = false;
    };
  }, []);

  // GitHub is read when the tab is shown and every 2 minutes while it is and the window is in
  // view (never in the background).
  useEffect(() => {
    const read = () => {
      if (document.visibilityState === "hidden") return;
      refreshPrs().catch((e) => toast("error", `Couldn't read GitHub: ${errText(e)}`, 5000, "prs-refresh"));
    };
    read();
    const t = setInterval(read, REFRESH_EVERY_MS);
    const shown = () => { if (document.visibilityState === "visible") read(); };
    document.addEventListener("visibilitychange", shown);
    return () => {
      clearInterval(t);
      document.removeEventListener("visibilitychange", shown);
    };
  }, []);

  const filter = prsFilter.value;
  const kept = mergedHere.value;
  // Cards merged from here keep their places in the strip, so it never shifts under the pointer.
  const waiting = kept.size ? sortWaiting(prs.value.values(), kept) : waitingPrs.value;
  const merged = mergedPrs.value;
  const list = filter === "waiting" ? waiting : merged;
  const chosen = selectedPr.value === null ? undefined : prs.value.get(selectedPr.value);
  // The one picked, even just merged or closed (its page says how it went), else the first of the list.
  const current = chosen && cardState(chosen) !== "gone" ? chosen : list[0];
  // A reorder of the list moves what is under the pointer: Merge ignores clicks for a moment.
  const order = waiting.map((p) => p.id).join(",");
  const inView = prsInView.value;
  // New cards not looked at yet, but the one shown.
  const fresh = arrivedPrs.value.filter((p) => p.id !== current?.id);
  const freshKey = fresh.map((p) => `${p.id}:${p.post}`).join(",");
  const [said, setSaid] = useState("");

  // The card shown stays shown: one the list put first (nothing picked yet, or the one picked was
  // withdrawn) is held, so a new card sorting ahead of it doesn't take its place.
  useLayoutEffect(() => {
    if (current && selectedPr.peek() !== current.id) selectedPr.value = current.id;
  }, [current?.id]);
  // The card shown, on screen, is new no longer (as soon as it is drawn).
  useLayoutEffect(() => {
    if (current && (inView || !!ref.current?.getClientRects().length)) seenPr(current.id);
  }, [current?.id, current?.post, inView, freshKey]);
  // A new card takes the stage from one that is done with, unless something is in hand there;
  // otherwise it waits in the bar's cue. Before paint, so the old card isn't drawn again.
  useLayoutEffect(() => {
    if (!fresh.length || !makesWay(current)) return;
    const root = ref.current;
    // The pointer resting on the dock after a Merge is fine (no button under it; Merge on the new
    // card ignores clicks for a moment); on a button there ("Next waiting") it waits.
    if (zoom || menu || root?.querySelector(".pb-dock button:hover, .pb-dock :focus")) return;
    const to = fresh[0];
    const hadFocus = !!root?.querySelector(".pb-scroll")?.contains(document.activeElement);
    selectedPr.value = to.id;
    prsFilter.value = "waiting";
    root?.querySelector(".pb-scroll")?.scrollTo(0, 0);
    setSaid(`New pull request shown: ${target(to)}, ${titleOf(to)}`);
    if (hadFocus) focusTitle();
  }, [freshKey]);

  const pick = (p: Pr) => {
    selectedPr.value = p.id;
    prsFilter.value = p.status === "merged" ? "merged" : "waiting";
    setMenu(false);
    ref.current?.querySelector(".pb-scroll")?.scrollTo(0, 0);
    focusTitle();
  };
  const showFilter = (f: "waiting" | "merged") => {
    prsFilter.value = f;
    clearSettled();
    // From the lists as they are after the clear (a card merged from here leaves Waiting now).
    selectedPr.value = (f === "waiting" ? waitingPrs.peek() : merged)[0]?.id ?? null;
    setMenu(false);
  };
  const openZoom = (p: Pr, index: number) => {
    setZoom({ id: p.id, post: p.post, index });
  };
  // The picture open large, while its card still shows that post and that picture.
  const zoomCard = zoom ? prs.value.get(zoom.id) : undefined;
  const zoomed = zoom && zoomCard && zoomCard.post === zoom.post && zoomCard.pictures?.[zoom.index] ? zoomCard : undefined;
  useEffect(() => {
    if (zoom && !zoomed) {
      setZoom(null);
      guardBump.value++;
    }
  }, [zoom, zoomed]);
  const toggleMenu = () => {
    if (menu) guardBump.value++;
    setMenu(!menu);
  };
  const closeMenu = (refocus: boolean) => {
    setMenu(false);
    guardBump.value++;
    if (refocus) requestAnimationFrame(() => ref.current?.querySelector<HTMLElement>(".pb-pager button")?.focus());
  };
  // While the large view is open the rest of the tab is out of reach (a modal dialog).
  const inert = { inert: !!zoomed };

  return (
    <div class={cx("questions-view", "prs-view", narrow && "narrow", short && "short")} ref={ref} tabIndex={-1}
      aria-label="PRs to merge">
      <div class="qv-bar" role="toolbar" aria-label="Which pull requests" {...inert}>
        <h1>PRs to merge</h1>
        <button type="button" class={cx("qv-chip", filter === "waiting" && "on")} aria-pressed={filter === "waiting"}
          onClick={() => showFilter("waiting")}>
          Waiting <span class="qv-n">{waitingPrs.value.length}</span>
        </button>
        <button type="button" class={cx("qv-chip", filter === "merged" && "on")} aria-pressed={filter === "merged"}
          onClick={() => showFilter("merged")}>
          Merged <span class="qv-n">{merged.length}</span>
        </button>
        {fresh.length > 0 && (
          <button type="button" class="pb-new" data-tip={`${target(fresh[0])} · ${titleOf(fresh[0])}`}
            aria-label={`${fresh.length} new: ${target(fresh[0])}, ${titleOf(fresh[0])}. Show it`} onClick={() => pick(fresh[0])}>
            <span class="pb-new-n">{fresh.length} new:</span> <span class="pb-new-t">{titleOf(fresh[0])}</span>
          </button>
        )}
        {list.length > 0 && (
          <span class="pb-pager">
            <button type="button" aria-expanded={menu} aria-controls="prs-all" onClick={toggleMenu}>
              All {list.length} with titles ▾
            </button>
          </span>
        )}
      </div>
      <span class="sr-only" role="status" aria-live="polite">
        {fresh.length ? `${fresh.length} new pull request${fresh.length === 1 ? "" : "s"}: ${target(fresh[0])}, ${titleOf(fresh[0])}` : said}
      </span>
      <div class="pb-wrap" {...inert}>
        {list.length > 0 && <Strip list={list} current={current} fresh={fresh} onPick={pick} />}
        {current
          ? (
            <div class="pb-scroll">
              <Stage key={`${current.id}`} p={current} now={now} onZoom={(index) => openZoom(current, index)} />
              <Dock key={`dock-${current.id}-${current.post}`} p={current} now={now} order={order} onPick={pick} />
            </div>
          )
          : (
            <div class="qv-empty qv-empty-page">
              {filter === "waiting"
                ? "No pull requests are waiting for you. When an agent has one ready for your merge, its card shows here with before and after pictures."
                : "Nothing merged from here yet."}
            </div>
          )}
        {menu && <AllMenu list={list} filter={filter} current={current} now={now} onPick={pick} onClose={closeMenu} />}
      </div>
      {zoom && zoomed && (
        <Lightbox p={zoomed} index={zoom.index} onStep={(index) => setZoom({ ...zoom, index })}
          onClose={() => { setZoom(null); guardBump.value++; }} />
      )}
    </div>
  );
}

/** One pill per pull request; one Tab stop (← → move and pick, Home and End jump). Each pill
 * carries a mark (✓ ○ … ! ✕), not colour alone, and says its title and state to a screen reader. */
function Strip({ list, current, fresh, onPick }: { list: Pr[]; current: Pr | undefined; fresh: Pr[]; onPick: (p: Pr) => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.querySelector<HTMLElement>(".pb-pill.on")?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [current?.id]);
  // A new card's pill comes into view (marked .new until it is looked at). Only the strip scrolls.
  const newest = fresh.at(-1);
  const newestKey = newest ? `${newest.id}:${newest.post}` : "";
  useEffect(() => {
    const strip = ref.current, pill = newest && strip?.querySelector<HTMLElement>(`#prs-pill-${newest.id}`);
    if (!strip || !pill) return;
    const s = strip.getBoundingClientRect(), r = pill.getBoundingClientRect();
    const by = r.left < s.left ? r.left - s.left - 14 : r.right > s.right ? r.right - s.right + 14 : 0;
    if (by) strip.scrollBy({ left: by, behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
  }, [newestKey]);
  const onKey = (e: KeyboardEvent) => {
    const i = list.findIndex((p) => p.id === current?.id);
    const to = e.key === "ArrowRight" ? Math.min(list.length - 1, i + 1) : e.key === "ArrowLeft" ? Math.max(0, i - 1)
      : e.key === "Home" ? 0 : e.key === "End" ? list.length - 1 : null;
    if (to === null) return;
    e.preventDefault();
    onPick(list[to]);
    requestAnimationFrame(() => ref.current?.querySelector<HTMLElement>(".pb-pill.on")?.focus());
  };
  return (
    <div class="pb-strip" role="tablist" aria-label="Pull requests" ref={ref} onKeyDown={onKey}>
      {list.map((p) => {
        const on = p.id === current?.id;
        const mark = queuedMark(p) ?? pillMark(p);
        return (
          <button key={p.id} id={`prs-pill-${p.id}`} type="button" role="tab" aria-selected={on} aria-controls="prs-stage"
            tabIndex={on || (!list.some((x) => x.id === current?.id) && p === list[0]) ? 0 : -1}
            aria-label={`#${p.number} ${p.repo}: ${titleOf(p)}. ${!on && fresh.some((x) => x.id === p.id) ? "New. " : ""}${mark.status}`}
            class={cx("pb-pill", on && "on", p.status === "merged" && "done", p.later != null && p.status !== "merged" && "later",
              !on && fresh.some((x) => x.id === p.id) && "new")}
            data-tip={`${p.repo} · ${titleOf(p)}`}
            onClick={() => { if (!on) onPick(p); }}>
            <span class={cx("pm-mark", mark.cls)} aria-hidden="true">{mark.glyph}</span>
            <span class="qv-qn">#{p.number}</span>{repoName(p)}
          </button>
        );
      })}
    </div>
  );
}

/** A card counting down or being sent from this window: its pill and list row say so. */
function queuedMark(p: Pr): { glyph: string; cls: "run"; status: string } | null {
  const q = queuedMerges.value.get(p.id);
  if (!q || p.status !== "waiting") return null;
  return { glyph: "…", cls: "run", status: q.phase === "count" ? "merging in a moment (Cancel is on its card)" : "merging" };
}

function AllMenu({ list, filter, current, now, onPick, onClose }: {
  list: Pr[]; filter: "waiting" | "merged"; current: Pr | undefined; now: number; onPick: (p: Pr) => void;
  onClose: (refocus: boolean) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    (ref.current?.querySelector<HTMLElement>(".qv-row.sel") ?? ref.current?.querySelector<HTMLElement>(".qv-row"))?.focus();
    const away = (e: PointerEvent) => {
      if (!ref.current?.contains(e.target as Node) && !(e.target as HTMLElement).closest?.(".pb-pager")) onClose(false);
    };
    addEventListener("pointerdown", away, true);
    return () => removeEventListener("pointerdown", away, true);
  }, []);
  const groups: [string, Pr[]][] = filter === "waiting"
    ? GROUPS.map(([g, title]) => [title, list.filter((p) => groupOf(p) === g)])
    : [["", list]];
  const onKey = (e: KeyboardEvent) => {
    const rows = [...(ref.current?.querySelectorAll<HTMLElement>(".qv-row") ?? [])];
    const i = rows.indexOf(document.activeElement as HTMLElement);
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); onClose(true); }
    else if (e.key === "ArrowDown") { e.preventDefault(); rows[Math.min(rows.length - 1, i + 1)]?.focus(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); rows[Math.max(0, i - 1)]?.focus(); }
  };
  // The keyboard leaving the list closes it.
  const onFocusOut = (e: FocusEvent) => {
    const to = e.relatedTarget as HTMLElement | null;
    if (to && !ref.current?.contains(to) && !to.closest(".pb-pager")) onClose(false);
  };
  return (
    <div class="pb-menu" id="prs-all" ref={ref} onKeyDown={onKey} onFocusOut={onFocusOut}>
      <div class="qv-list" role="list" aria-label="All pull requests">
        {groups.filter(([, ps]) => ps.length).map(([title, ps]) => (
          <div key={title} role="group" aria-label={title || undefined}>
            {title && <div class="qv-grp">{title}</div>}
            {ps.map((p) => {
              const q = queuedMark(p);
              const tag = q ? { text: q.status.startsWith("merging in") ? "merging soon" : "merging", tone: "" } : tagText(p);
              return (
                <div key={p.id} role="listitem">
                  <button type="button" class={cx("qv-row", "pm-row", p.id === current?.id && "sel", p.status === "merged" && "done")}
                    aria-current={p.id === current?.id ? "true" : undefined} onClick={() => onPick(p)}>
                    <span class="qv-top">
                      <span class="qv-qn">#{p.number}</span><span class="qv-ttl">{titleOf(p)}</span>
                      <span class={cx("pm-tag", tag.tone)}>{tag.text}</span>
                    </span>
                    <span class="qv-sub">
                      <span>{p.repo}</span><span aria-hidden="true">·</span><span>{p.poster.label}</span>
                      {p.status !== "merged" && <><span aria-hidden="true">·</span><span>{ageText(now - p.created_at)}</span></>}
                    </span>
                  </button>
                </div>
              );
            })}
          </div>
        ))}
      </div>
    </div>
  );
}

function Stage({ p, now, onZoom }: { p: Pr; now: number; onZoom: (index: number) => void }) {
  const b = banner(p, (at) => clockText(at, now));
  const s = cardState(p);
  const noChecks = p.verdict?.checks === "none";
  return (
    <div class="pb-stage" id="prs-stage" role="tabpanel" aria-labelledby={`prs-pill-${p.id}`}>
      <div class="qv-meta">
        <span class="qv-prod">{p.repo}</span>
        <span class="qv-qn">#{p.number}</span>
        <span class="qv-who">{p.poster.label}</span>
        <span>posted {clockText(p.created_at, now)}{s !== "merged" ? `, ${ageText(now - p.created_at)} ago` : ""}</span>
        {p.later != null && s !== "merged" && <span class="pm-tag">Not now</span>}
      </div>
      {/* Non-breaking hyphens: a balanced title never breaks "sign-up" across lines. */}
      <h2 class="qv-q" tabIndex={-1}>{nb(titleOf(p))}</h2>
      {s === "merged" && <MergedBlock p={p} now={now} />}
      {b && (
        <div class={cx("pm-banner", b.tone)} role="status">
          <b>{b.head}</b>
          <span>{nb(b.body)}</span>
        </div>
      )}
      <p class="qv-ctx"><LinkedText from={PRS_TAB} text={p.summary} /></p>
      <p class="qv-ex"><b>Who notices</b><LinkedText from={PRS_TAB} text={p.who_notices} /></p>
      <Pictures p={p} stale={s === "stale"} onZoom={onZoom} />
      <section class="pm-words" aria-label={`${p.poster.label}'s words`}>
        <div class="pm-h">{p.poster.label}'s words</div>
        <div class="qv-fate">
          <div>
            <span>Tests it ran</span>
            <span><LinkedText from={PRS_TAB} text={p.tests} />
              {noChecks && <span class="pm-only">The only test evidence: GitHub runs no checks here.</span>}</span>
          </div>
          <div><span>How to undo</span><span><LinkedText from={PRS_TAB} text={p.undo} /></span></div>
          <div><span>After the merge</span><span><LinkedText from={PRS_TAB} text={p.after_merge} /></span></div>
        </div>
      </section>
      <details class="qv-details">
        <summary>Details</summary>
        <p>Posted for commit {p.head_sha}{p.github?.head_sha && p.github.head_sha !== p.head_sha ? `\nGitHub's newest: ${p.github.head_sha}` : ""}</p>
        {p.details && <p class="pm-details-text">{p.details}</p>}
        {p.refused?.github_said && <p>GitHub said: {p.refused.github_said}</p>}
        {p.read_error && <p>The last read failed ({clockText(p.read_error.at, now)}): {p.read_error.text}</p>}
        {(p.pictures ?? []).map((x, i) => <p key={i}>Picture {i + 1} ({x.side}): {x.path}</p>)}
        <p>{mergeCommand(p)}</p>
      </details>
    </div>
  );
}

function MergedBlock({ p, now }: { p: Pr; now: number }) {
  const commit = p.merge?.commit;
  return (
    <div class="qv-answered">
      <div class="qv-ah">Merged</div>
      <div class="qv-a">
        {target(p)} is in {baseName(p)}{commit ? <> as commit <span class="mono">{shortSha(commit)}</span></> : ""}
      </div>
      <div class="qv-words">
        Merged at {clockText(p.merge?.done_at ?? p.updated_at, now)} by your click. {nb(toldText(p, "merged"))}; what comes after
        the merge: “{p.after_merge}”
      </div>
    </div>
  );
}

function Pictures({ p, stale, onZoom }: { p: Pr; stale: boolean; onZoom: (index: number) => void }) {
  const list = p.pictures ?? [];
  // A picture left out by post_pr is never asked for; one Muster sends only the size of (a card
  // stored before that rule) is left out once it arrives.
  useEffect(() => list.forEach((x, i) => !x.too_tall && loadPrPicture(p, i)), [p.id, p.post, list.length]);
  const all = pairs(p);
  if (!all.length) return null;
  const out = leftOut(p);
  // A left-out picture keeps its place in the pair: its label, then a line saying why, so the
  // picture on the other side shows alone and the other pairs stay as posted.
  const fig = (i: number | null) => {
    if (i === null) return null;
    const x = list[i];
    const got = prPictures.value.get(pictureKey(p, i));
    // "older version" marks a picture shown; a left-out one has only its line.
    const label = (
      <div class={cx("pm-side", x.side)}>
        {x.side === "before" ? "Before" : "After"}
        {stale && !out(i) && <span class="pm-tag warn pm-old">older version</span>}
      </div>
    );
    if (out(i)) {
      return (
        <figure class="pm-fig">
          {label}
          <p class="pm-left-out">{leftOutText(x.caption, x.too_tall ?? got?.tooTall, p.poster.label, !!x.too_tall)}</p>
        </figure>
      );
    }
    return (
      <figure class="pm-fig">
        {label}
        {got?.src
          ? (
            <button type="button" class="pm-shot" aria-label={`Show full size: ${x.side}, ${x.caption}`} onClick={() => onZoom(i)}>
              <img src={got.src} alt={x.caption} />
            </button>
          )
          : <div class="qv-shot-missing">{got?.error ? `Can't show this picture: ${got.error}` : "Loading the picture…"}</div>}
        <figcaption>{x.caption}</figcaption>
      </figure>
    );
  };
  return (
    <div class="pm-pairs">
      {all.map((pair, k) => (
        <div key={k} class={cx("pm-pair", (pair.before === null || pair.after === null) && "solo")}>
          {fig(pair.before)}{fig(pair.after)}
        </div>
      ))}
    </div>
  );
}

function Ticks({ list }: { list: Rule[] }) {
  const cls = { pass: "", fail: "no", warn: "warn", wait: "run", none: "none" } as const;
  const sr = { pass: "Passes:", fail: "Fails:", warn: "Not yet:", wait: "Still running:", none: "Nothing to check:" } as const;
  return (
    <ul class="pb-ticks" aria-label="Merge rules">
      {list.map((r, i) => (
        <li key={i} class={cls[r.mark]}>
          <span class="sr-only">{sr[r.mark]} </span>
          <span>{r.text}{r.sha && <> <span class="mono">{r.sha}</span></>}</span>
        </li>
      ))}
    </ul>
  );
}

type Phase = { kind: "idle" } | { kind: "ask" };

function Dock({ p, now, order, onPick }: { p: Pr; now: number; order: string; onPick: (p: Pr) => void }) {
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });
  const [busy, setBusy] = useState(false);
  const [reading, setReading] = useState(false);
  const [hint, setHint] = useState("");
  const [words, setWords] = useState("");
  const [mergeWidth, setMergeWidth] = useState(0);
  const guard = useRef(performance.now());
  const hintTimer = useRef<ReturnType<typeof setTimeout>>();
  const say = (text: string) => {
    clearTimeout(hintTimer.current);
    setHint(text);
    hintTimer.current = setTimeout(() => setHint(""), 3000);
  };
  useEffect(() => () => clearTimeout(hintTimer.current), []);
  const ref = useRef<HTMLDivElement>(null);
  const s = cardState(p);
  const can = canMerge(p);
  const who = p.poster.label;
  const bump = guardBump.value;
  // This card's countdown or send, if it has one (prs_merges.ts: it runs whether or not this
  // dock is shown).
  const queued = queuedMerges.value.get(p.id);
  const counting = queued?.phase === "count" && p.status === "waiting";
  // Why this card's last send from here failed (for this post and commit), until Merge is clicked again.
  const tried = failedMerges.value.get(p.id);
  const failed = tried && tried.post === p.post && tried.sha === p.head_sha ? tried : undefined;
  // Re-renders the seconds left while it counts down.
  useNow(counting ? 200 : 60_000);

  // Merge ignores clicks for a moment after its card appears or changes, the list reorders, the
  // large view or list closes, or Merge comes back in Cancel's place (a send that failed). Before
  // paint, so no click lands in between.
  useLayoutEffect(() => { guard.current = performance.now(); }, [p.updated_at, p.head_sha, p.post, order, bump, !!queued]);

  // Esc anywhere in the tab cancels the countdown of the card shown, unless the large view or the
  // list is open (Esc closes that).
  useEffect(() => {
    if (!counting) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.repeat || !(e.target as HTMLElement)?.closest?.(".prs-view")) return;
      if (document.querySelector(".prs-view .pm-lightbox, .prs-view .pb-menu")) return;
      e.preventDefault();
      e.stopPropagation();
      cancel();
    };
    addEventListener("keydown", onKey, true);
    return () => removeEventListener("keydown", onKey, true);
  }, [counting]);

  const start = () => {
    if (!can || queued || !p.github) return;
    if (performance.now() - guard.current < CLICK_GUARD_MS) {
      say("That click came right after the card changed, so Merge ignored it: click again.");
      return;
    }
    setMergeWidth(ref.current?.querySelector<HTMLElement>(".pm-merge")?.offsetWidth ?? 0);
    // The card stays shown through its result, even when it leaves the Waiting list.
    selectedPr.value = p.id;
    startMerge({ id: p.id, sha: p.head_sha, base: p.github.base, post: p.post });
    requestAnimationFrame(() => ref.current?.querySelector<HTMLElement>(".pm-cancel")?.focus());
  };
  function cancel() {
    if (!cancelMerge(p.id)) return;
    guard.current = performance.now();
    focusTitle();
  }

  const checkAgain = async () => {
    if (reading) return;
    setReading(true);
    try {
      await refreshPrs(p.id);
    } catch (e) {
      toast("error", `Couldn't read ${target(p)}: ${errText(e)}`);
    } finally {
      setReading(false);
    }
  };

  const later = async () => {
    if (busy) return;
    setBusy(true);
    const before = waitingPrs.peek();
    try {
      const setAside = p.later == null;
      await setLater(p, setAside);
      // Set aside: on to the next waiting card (this one moves down the list).
      if (setAside) {
        const next = before.find((x) => x.id !== p.id && x.later == null);
        if (next) {
          selectedPr.value = next.id;
          focusTitle();
        }
      }
    } catch (e) {
      toast("error", errText(e));
    } finally {
      setBusy(false);
    }
  };

  const back = () => {
    setPhase({ kind: "idle" });
    requestAnimationFrame(() => ref.current?.querySelector<HTMLElement>(".pm-ask-btn")?.focus());
  };

  const sendAsk = async () => {
    const w = words.trim();
    if (!w || busy) return;
    setBusy(true);
    const before = waitingPrs.peek();
    try {
      await askChanges(p, w);
      toast("success", `Your words are on their way to ${who}; ${target(p)} left the list until it is posted again`, 6000);
      const next = before.find((x) => x.id !== p.id);
      selectedPr.value = next?.id ?? null;
      focusTitle();
    } catch (e) {
      toast("error", `Couldn't send it: ${errText(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const session = posterSession(p);
  const links = (
    <span class="qv-links">
      {p.github?.url && (
        <button type="button" class="qv-link" data-tip={p.github.url} data-tip-focus
          onClick={() => globalThis.open(p.github!.url, "_blank", "noopener,noreferrer")}>
          Open on GitHub
        </button>
      )}
      {session && <button type="button" class="qv-link" onClick={() => A.gotoSession(session.id)}>Open {who}'s tab</button>}
    </span>
  );

  if (s === "merged" || s === "closed") {
    // Not one already counting down or being sent from here.
    const next = nextReady(waitingPrs.value.filter((x) => x.id === p.id || !queuedMerges.value.has(x.id)), p);
    return (
      <div class="pb-dock" ref={ref}>
        <div class="pb-dock-in">
          <div class="pb-btns pb-start">
            {s === "merged" && (next
              ? <button type="button" class="qv-btn" onClick={() => onPick(next)}>Next waiting: {target(next)} ›</button>
              : <span class="pb-fine">Nothing else is ready to merge.</span>)}
          </div>
          <div class="pb-fine pb-end">{links}</div>
        </div>
      </div>
    );
  }

  if (phase.kind === "ask") {
    return (
      <div class="pb-dock" ref={ref}>
        <div class="pb-dock-in pb-one">
          <div class="pm-ask">
            <label for={`pm-ask-${p.id}`}>
              What should {nb(who)} change? Your words go to {nb(who)}, and this card leaves the list until it is posted again.
            </label>
            <textarea id={`pm-ask-${p.id}`} rows={3} placeholder="Say what to change" value={words}
              onInput={(e) => setWords((e.target as HTMLTextAreaElement).value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) { e.preventDefault(); void sendAsk(); }
                else if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); back(); }
              }} />
          </div>
          <div class="pb-btns pb-start">
            <button type="button" class="qv-btn primary pm-send" disabled={!words.trim() || busy} onClick={() => void sendAsk()}
              data-keys="Ctrl+Enter">
              Send to {who}
            </button>
            <button type="button" class="qv-btn" onClick={back}>Back</button>
            <span class="pm-target pm-ask-hint">Ctrl+Enter sends. Nothing is merged.</span>
          </div>
        </div>
      </div>
    );
  }

  // Idle, counting down, or asking GitHub: one layout, so Cancel sits exactly where Merge was
  // (Ask for changes and Not now keep their places, hidden).
  const sending = (queued != null && !counting) || s === "merging";
  const going = counting || sending;
  const seenSha = queued?.seen.sha ?? p.merge?.sha ?? p.head_sha;
  const leftMs = counting && queued ? Math.max(0, Math.min(COUNTDOWN_S * 1000, queued.endsAt - Date.now())) : 0;
  const left = Math.ceil(leftMs / 1000);
  const list = rules(p);
  const size = sizeOf(p);
  const off = offReason(p);
  const status = counting ? `Merging in ${left} s` : sending ? "Asking GitHub to merge" : "";
  const spoken = going ? `${status}: ${target(p)}, commit ${shortSha(seenSha)}, into ${baseName(p)}` : "";
  return (
    <div class={cx("pb-dock", going && "counting")} ref={ref}>
      {going && <div class="pm-drain" aria-hidden="true"><i style={{ width: `${(leftMs / (COUNTDOWN_S * 1000)) * 100}%` }} /></div>}
      <span class="sr-only" role="status" aria-live="polite">{spoken}</span>
      <div class="pb-dock-in">
        {/* While it counts down, the idle words keep their places (hidden), so every row, and Cancel
            in Merge's place, stays exactly where it was; the countdown's words lie over them. */}
        <div class="pb-dock-h">
          {going && (
            <span class="pb-big pm-over" aria-hidden="true">
              {status} <span class="pm-over-small">{target(p)}, commit <span class="mono">{shortSha(seenSha)}</span></span>
            </span>
          )}
          <span class={cx("pm-idle", going && "pm-held")}>
                <span class="pm-h">Read from GitHub by Muster</span>
                {p.github && <span>{clockText(p.github.at, now)}, {ageText(now - p.github.at)}{now - p.github.at >= 60_000 ? " ago" : ""}</span>}
                {p.read_error && <span class="pm-warn-text">The last read failed: {p.read_error.text}</span>}
                <button type="button" class="qv-link" aria-disabled={reading} onClick={() => void checkAgain()}>
                  {reading ? "Reading…" : "Check again"}
                </button>
                {size && (
                  <span class="what">
                    {size.files} · <span class="pm-add">{size.add}</span> <span class="pm-del">{size.del}</span>
                  </span>
                )}
          </span>
        </div>
        {list ? <Ticks list={list} /> : <p class="pb-reading">{p.read_error ? "Muster couldn't read it from GitHub." : "Reading GitHub…"}</p>}
        <div class="pb-btns">
          <button type="button" class={cx("qv-btn", "pm-ask-btn", going && "pm-held")} aria-disabled={busy} tabIndex={going ? -1 : 0}
            onClick={() => {
              if (busy || going) return;
              setWords("");
              setPhase({ kind: "ask" });
              requestAnimationFrame(() => ref.current?.querySelector<HTMLElement>(".pm-ask textarea")?.focus());
            }}>
            Ask for changes
          </button>
          <button type="button" class={cx("qv-btn", going && "pm-held")} aria-disabled={busy} tabIndex={going ? -1 : 0}
            onClick={() => { if (!going) void later(); }}>
            {p.later == null ? "Not now" : "Undo Not now"}
          </button>
          {counting
            ? (
              <button type="button" class="qv-btn pm-cancel" style={mergeWidth ? { width: `${mergeWidth}px`, minWidth: 0 } : undefined}
                onClick={() => cancel()}>
                Cancel
              </button>
            )
            : sending
            ? (
              <button type="button" class="qv-btn pm-cancel" disabled style={mergeWidth ? { width: `${mergeWidth}px`, minWidth: 0 } : undefined}>
                Merging…
              </button>
            )
            : (
              <button type="button" class="qv-btn primary pm-merge" disabled={!can}
                data-tip={can ? `Starts a ${COUNTDOWN_S} second countdown that keeps going when you move on; Cancel takes this button's place` : undefined}
                data-keys={can ? "Ctrl+Enter" : undefined}
                onClick={(e) => { if (e.detail >= 1 && e.button === 0) start(); }}
                onKeyDown={(e) => {
                  if (e.repeat) { e.preventDefault(); return; }
                  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) { e.preventDefault(); start(); }
                  else if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    say("Ctrl+Enter starts Merge; Enter and Space do nothing here.");
                  }
                }}>
                <IconMerge />Merge {target(p)}
              </button>
            )}
        </div>
        <div class="pb-fine">
          {going && (
            <span class="pm-over" aria-hidden="true">
              Into {baseName(p)} as a merge commit. {counting ? "Cancel or Esc stops it. You can move on: Muster asks GitHub to merge when the countdown ends." : "You can move on meanwhile."}
            </span>
          )}
          <span class={cx("pm-idle", going && "pm-held")}>
            {hint
              ? <span class="off" role="status">{hint}</span>
              : failed && can
              ? <span class="off" role="status">{failed.lost ? "Outcome unknown" : "Not merged"}: {failed.text}</span>
              : can
              ? <span class="pm-fine-text">A merge commit into {baseName(p)}; Muster leaves the branch alone. You get {COUNTDOWN_S} seconds to cancel, and can move on meanwhile.</span>
              : off && <span class="off">Merge is off: {off}.</span>}
            {links}
          </span>
        </div>
      </div>
    </div>
  );
}

function Lightbox({ p, index, onStep, onClose }: { p: Pr; index: number; onStep: (i: number) => void; onClose: () => void }) {
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const opener = document.activeElement as HTMLElement | null;
    box.current?.querySelector<HTMLElement>(".qv-lightbox-close")?.focus();
    return () => opener?.isConnected && opener.focus();
  }, []);
  const x = (p.pictures ?? [])[index];
  if (!x) return null;
  const got = prPictures.value.get(pictureKey(p, index));
  const out = leftOut(p);
  const other = otherSide(p, index, out);
  const seg = (side: "before" | "after") => {
    const on = x.side === side;
    return (
      <button type="button" class={cx(on && "on")} aria-pressed={on} disabled={!on && other === null}
        onClick={() => { if (!on && other !== null) onStep(other); }}>
        {side === "before" ? "Before" : "After"}
      </button>
    );
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); onClose(); }
    else if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
      e.preventDefault();
      onStep(stepPicture(p, index, e.key === "ArrowRight" ? 1 : -1, out));
    } else if (e.key === "Tab") {
      // The keyboard stays inside the dialog.
      const all = [...(box.current?.querySelectorAll<HTMLElement>("button:not(:disabled)") ?? [])];
      const i = all.indexOf(document.activeElement as HTMLElement);
      e.preventDefault();
      all[(i + (e.shiftKey ? -1 : 1) + all.length) % all.length]?.focus();
    }
  };
  // A click anywhere but the picture and the controls closes it.
  const stop = (e: Event) => e.stopPropagation();
  return (
    <div class="qv-lightbox pm-lightbox" role="dialog" aria-modal="true" aria-label={`${x.side === "before" ? "Before" : "After"}: ${x.caption}`}
      ref={box} onClick={onClose} onKeyDown={onKey}>
      <div class="pm-box-bar" onClick={stop}>
        <span class="pm-seg" role="group" aria-label="Which picture">{seg("before")}{seg("after")}</span>
        <span class="pm-box-text">{target(p)} · {pairText(p, index)} · ← → switch</span>
      </div>
      <div class="pm-zoomed">
        {got?.src
          ? <img src={got.src} alt={x.caption} onClick={stop} />
          : (
            <div class="qv-shot-missing" onClick={stop}>
              {got?.tooTall || x.too_tall
                ? leftOutText(x.caption, x.too_tall ?? got?.tooTall, p.poster.label, !!x.too_tall)
                : got?.error ?? "Loading the picture…"}
            </div>
          )}
      </div>
      <div class="pm-box-bar" onClick={stop}>
        <span class="pm-box-text pm-cap">{x.caption}</span>
      </div>
      <div class="pm-box-bar" onClick={stop}>
        <button type="button" class="qv-btn" onClick={() => onStep(stepPicture(p, index, -1, out))}>‹ Previous</button>
        <button type="button" class="qv-btn" onClick={() => onStep(stepPicture(p, index, 1, out))}>Next ›</button>
        <button type="button" class="qv-btn qv-lightbox-close" onClick={onClose}>Close (Esc)</button>
      </div>
    </div>
  );
}
