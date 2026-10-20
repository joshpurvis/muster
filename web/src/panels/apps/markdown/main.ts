// The markdown app: the agent's Markdown set for reading (a 70-character measure, 15 px text),
// and a box to send the agent a comment. A small, safe subset of Markdown, built as DOM nodes
// (never as HTML): headings, paragraphs, lists and checklists, quotes, code blocks with their
// language, tables, rules, and inline code, bold, italic and http(s) links.
import { openLink, sendText, startApp } from "../shared/client.ts";
import { iconEl } from "../../kit/icons.ts";

const el = (tag: string, text?: string, cls?: string) => {
  const e = document.createElement(tag);
  if (text !== undefined) e.textContent = text;
  if (cls) e.className = cls;
  return e;
};

/** Inline Markdown: `code`, **bold**, *italic*, [text](https://…). */
export function inline(parent: Node, text: string, depth = 0) {
  if (depth > 4) { parent.appendChild(document.createTextNode(text)); return; }
  const re = /(`[^`]+`|\*\*[^*]+\*\*|\*[^*]+\*|\[[^\]]+\]\([^\s)]+\))/g;
  let start = 0;
  for (const match of text.matchAll(re)) {
    parent.appendChild(document.createTextNode(text.slice(start, match.index)));
    const token = match[0];
    if (token[0] === "`") parent.appendChild(el("code", token.slice(1, -1)));
    else if (token[0] === "[") {
      const cut = token.indexOf("](");
      const url = token.slice(cut + 2, -1);
      if (/^https?:\/\//i.test(url)) {
        const a = el("a", token.slice(1, cut)) as HTMLAnchorElement;
        a.href = url;
        a.addEventListener("click", (e) => { e.preventDefault(); openLink(url).catch(() => {}); });
        parent.appendChild(a);
      } else parent.appendChild(document.createTextNode(token.slice(1, cut)));
    } else {
      const strong = token.startsWith("**");
      const e = el(strong ? "strong" : "em");
      inline(e, token.slice(strong ? 2 : 1, strong ? -2 : -1), depth + 1);
      parent.appendChild(e);
    }
    start = match.index! + token.length;
  }
  parent.appendChild(document.createTextNode(text.slice(start)));
}

const NUMERIC = /^[−+-]?[$€£]?\d[\d,.]*\s?(%|ms|s|m|h|KB|MB|GB|KiB|MiB|k|M|B|×|x)?$/;
const BLOCK_START = /^(#{1,6}\s|\s*```|\s*[-*+] |\s*\d+\. |\s*>|\s*([-*_])\s*\2\s*\2[\s\-*_]*$)/;

export function render(root: HTMLElement, text: string) {
  root.replaceChildren();
  const lines = String(text || "").replace(/\r/g, "").split("\n");
  const cells = (line: string) => line.trim().replace(/^\|/, "").replace(/\|$/, "").split("|").map((x) => x.trim());
  let i = 0;
  let afterTitle = false;
  while (i < lines.length) {
    const line = lines[i++];
    if (!line.trim()) continue;
    const wasTitle = afterTitle;
    afterTitle = false;
    const fence = /^\s*```\s*([\w+#.-]{0,20})/.exec(line);
    if (fence) {
      const block: string[] = [];
      while (i < lines.length && !/^\s*```/.test(lines[i])) block.push(lines[i++]);
      i++;
      const pre = el("pre");
      if (fence[1]) pre.appendChild(el("span", fence[1], "lang"));
      const code = el("code");
      block.forEach((l, n) => {
        // Comment lines read quieter; everything else stays plain text.
        code.appendChild(/^\s*(#|\/\/|--)\s/.test(l) || /^\s*#$/.test(l) ? el("span", l, "c") : document.createTextNode(l));
        if (n < block.length - 1) code.appendChild(document.createTextNode("\n"));
      });
      pre.appendChild(code);
      root.appendChild(pre);
      continue;
    }
    if (line.includes("|") && i < lines.length && /^\s*\|?\s*:?-{3,}/.test(lines[i])) {
      const aligns = cells(lines[i]).map((c) => c.endsWith(":") ? (c.startsWith(":") ? "center" : "right") : "");
      i++;
      const table = el("table"), thead = el("thead"), tbody = el("tbody"), head = el("tr");
      const rows: string[][] = [];
      while (i < lines.length && lines[i].includes("|")) rows.push(cells(lines[i++]));
      // A column of numbers (with units) reads right-aligned in tabular figures.
      const numeric = cells(line).map((_, c) => rows.length > 0 && rows.every((r) => !r[c] || NUMERIC.test(r[c])));
      cells(line).forEach((cell, c) => {
        const th = el("th");
        if (aligns[c] || numeric[c]) th.style.textAlign = aligns[c] || "right";
        inline(th, cell);
        head.appendChild(th);
      });
      thead.appendChild(head);
      for (const r of rows) {
        const tr = el("tr");
        r.forEach((cell, c) => {
          const td = el("td", undefined, numeric[c] ? "n" : undefined);
          if (aligns[c]) td.style.textAlign = aligns[c];
          inline(td, cell);
          tr.appendChild(td);
        });
        tbody.appendChild(tr);
      }
      table.append(thead, tbody);
      const wrap = el("div", undefined, "table-wrap");
      wrap.appendChild(table);
      root.appendChild(wrap);
      continue;
    }
    if (/^\s*([-*_])\s*\1\s*\1[\s\-*_]*$/.test(line)) { root.appendChild(el("hr")); continue; }
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      const hd = el("h" + heading[1].length);
      inline(hd, heading[2]);
      root.appendChild(hd);
      afterTitle = heading[1].length === 1;
      continue;
    }
    if (/^\s*>/.test(line)) {
      const quote = [line.replace(/^\s*>\s?/, "")];
      while (i < lines.length && /^\s*>/.test(lines[i])) quote.push(lines[i++].replace(/^\s*>\s?/, ""));
      const bq = el("blockquote");
      inline(bq, quote.join("\n"));
      root.appendChild(bq);
      continue;
    }
    const list = /^\s*([-*+] |\d+\. )(.*)$/.exec(line);
    if (list) {
      const ordered = /^\d/.test(list[1]);
      const ul = el(ordered ? "ol" : "ul");
      let value = list[2];
      let tasks = 0, items = 0;
      for (;;) {
        const li = el("li");
        const task = !ordered ? /^\[([ xX])\]\s+(.*)$/.exec(value) : null;
        if (task) {
          tasks++;
          const on = task[1] !== " ";
          const box = el("span", undefined, `box${on ? " on" : ""}`);
          box.setAttribute("role", "img");
          box.setAttribute("aria-label", on ? "Done" : "Not done");
          if (on) box.appendChild(iconEl("check"));
          li.appendChild(box);
          inline(li, task[2]);
        } else inline(li, value);
        items++;
        ul.appendChild(li);
        const next = /^\s*([-*+] |\d+\. )(.*)$/.exec(lines[i] || "");
        if (!next || /^\d/.test(next[1]) !== ordered) break;
        i++;
        value = next[2];
      }
      if (tasks === items) ul.classList.add("tasks");
      root.appendChild(ul);
      continue;
    }
    const p = el("p", undefined, wasTitle ? "lead" : undefined);
    const paragraph = [line];
    while (i < lines.length && lines[i].trim() && !BLOCK_START.test(lines[i]) && !(lines[i].includes("|") && /^\s*\|?\s*:?-{3,}/.test(lines[i + 1] ?? ""))) paragraph.push(lines[i++]);
    inline(p, paragraph.join("\n"));
    root.appendChild(p);
  }
}

if (typeof document !== "undefined" && document.getElementById("app")) {
  const main = document.getElementById("app")!;
  const article = el("article", undefined, "md");
  article.id = "content";
  const form = el("section", undefined, "comment");
  form.innerHTML = `<label for="comment">Send a comment to the agent</label><textarea id="comment" rows="2" maxlength="8000" placeholder="What should the agent change or know?"></textarea><div class="row"><button type="button" id="send-comment">Send comment</button><output role="status"></output></div>`;
  main.append(article, form);
  const input = form.querySelector("textarea")!;
  const status = form.querySelector("output")!;
  form.querySelector("button")!.addEventListener("click", () => {
    if (!input.value.trim()) { status.textContent = "Write a comment first."; return; }
    // Posted straight from the click: the host takes it only during a real user action.
    const sent = sendText(input.value);
    sent.then(() => { input.value = ""; status.textContent = "Sent. The agent has it."; }, (e) => { status.textContent = `Not sent: ${e.message}`; });
  });
  startApp({ name: "Muster Markdown", input: (data) => render(article, String((data as { markdown?: unknown })?.markdown ?? "")) });
}
