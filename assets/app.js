// Session ledger page. Every piece of session text goes into the DOM through
// textContent or value; the page never builds HTML from strings.
"use strict";

const $ = id => document.getElementById(id);

const MSG_EMPTY = 'No sessions yet. Run "sled sweep" to import existing sessions.';
const MSG_EMPTY_PERIOD = "No sessions in this period. Try a longer one.";
const MSG_NO_MATCH = "No sessions match. Clear the search or pick another project.";
const MSG_FAILED = 'Could not load sessions. Is "sled serve" still running?';
const LIST_BASIS = "Estimated at API list prices. On a subscription or enterprise plan this is not what you pay.";
const PARTIAL_TITLE = "Some usage has no known price";
const NO_RECAP = "This session was too short to get a recap. Its first and last asks are below.";
const COST_FALLBACK = "Estimated cost";
const NO_FOLDER_LABEL = "No folder";
const RESUME_FAILED = "Could not load this session. Close the drawer and try again.";
const RESUME_BLOCKED = new Map([
  ["transcript_removed", "Transcript removed, cannot resume. Claude Code deleted this session's file."],
  ["no_folder", "No folder was recorded for this session, so there is no command to resume it."],
  ["unsafe_folder", "This session's folder name has characters that are unsafe to paste, so no command is offered."],
  ["unsafe_id", "This session's id has unusual characters, so no command is offered."],
]);
const COPY_FAILED = "Could not copy. The command is selected; press Cmd+C or Ctrl+C.";
const NOTE_CONFLICT = "This note changed elsewhere. Edit it to save your version.";
const NOTE_PLACEHOLDER = "Add a note for when you come back";
const REFRESH_MS = 30000;

const DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS = ["January", "February", "March", "April", "May", "June", "July",
  "August", "September", "October", "November", "December"];

// ---- state ----
let sessions = [];     // the fetched list
let costMode = "list";
let costLabel = "";
let demo = false;      // the server replaces identifying text; notes are read-only
let period = "14";
let automated = false;
let project = null;    // selected project key (see projectKey), or null for all
let query = "";
let loadSeq = 0;       // ignores responses from superseded list fetches
let loading = false;   // a list fetch is in flight; the auto refresh waits for the next tick
let current = null;    // id of the session the drawer is open on or switching to
let shown = null;      // { id, failed } of the session the drawer has actually rendered
let returnFocusId = null;

// Project key for sessions with no recorded folder (empty project_dir). A Symbol, so
// no label can equal it, not even "" (the label of the root folder); it never enters the DOM.
const NO_FOLDER = Symbol("no folder");
const projectKey = s => s.project_dir ? String(s.project ?? "") : NO_FOLDER;
const projectName = key => key === NO_FOLDER ? NO_FOLDER_LABEL : key === "" ? "/" : key;
// A project button's key: null for "All projects", NO_FOLDER, or the label.
const buttonKey = b => b.dataset.kind === "all" ? null : b.dataset.kind === "none" ? NO_FOLDER : b.dataset.key;

// ---- small DOM helper ----
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined && text !== null) e.textContent = String(text);
  return e;
}

// ---- formatting ----
function dayKey(ms) {
  const d = new Date(ms);
  return d.getFullYear() + "-" + d.getMonth() + "-" + d.getDate();
}
function dayName(ms) {
  const d = new Date(ms);
  return DAYS[d.getDay()] + " " + d.getDate() + " " + MONTHS[d.getMonth()];
}
function clock(ms) {
  const d = new Date(ms);
  return String(d.getHours()).padStart(2, "0") + ":" + String(d.getMinutes()).padStart(2, "0");
}
function duration(ms) {
  const mins = Math.floor((ms || 0) / 60000);
  if (mins < 1) return "under 1m";
  if (mins < 60) return mins + "m";
  return Math.floor(mins / 60) + "h " + String(mins % 60).padStart(2, "0") + "m";
}
/// Short token count: exact below 1,000, then k, M and B. Each unit is rounded first,
/// so a value that rounds up to the next unit uses it (999,950 is "1.0M", not "1000k").
function tokens(n) {
  if (n < 1000) return String(n);
  const k = Math.round(n / 1e3);
  if (k < 1000) return k + "k";
  const m = Math.round(n / 1e5); // tenths of a million
  if (m < 10000) return (m / 10).toFixed(1) + "M";
  return (Math.round(n / 1e8) / 10).toFixed(1) + "B";
}
function money(n) {
  return "$" + n.toFixed(2);
}
function exact(n) {
  return Number(n || 0).toLocaleString("en-US");
}
function shortDir(dir) {
  return (dir || "").replace(/^\/(Users|home)\/[^/]+(?=\/|$)/, "~");
}

/// Sums costs over sessions: { text, partial, unknown } for a tile, day row or cell.
function costOf(list) {
  let sum = 0, known = 0, partial = false;
  for (const s of list) {
    if (s.cost === null || s.cost === undefined) { partial = true; continue; }
    sum += s.cost; known += 1;
    if (s.cost_partial) partial = true;
  }
  if (list.length > 0 && known === 0) return { text: "price unknown", partial: false, unknown: true };
  return { text: money(sum) + (partial ? "+" : ""), partial, unknown: false };
}

/// Fills `node` with a formatted cost and the partial-price hint where needed.
function putCost(node, c) {
  node.textContent = c.text;
  if (c.partial) node.title = PARTIAL_TITLE;
  if (c.unknown) node.classList.add("unknown");
  return node;
}

const showCost = () => costMode !== "tokens";
const narrow = matchMedia("(max-width: 900px)"); // matches the narrow rules in app.css
const totalTokens = list => list.reduce((a, s) => a + (s.tokens ? s.tokens.total : 0), 0);
const lastActive = s => s.last_active_at ?? s.started_at;

// ---- list ----
function visible() {
  const q = query.trim().toLowerCase();
  return sessions.filter(s => (project === null || projectKey(s) === project) &&
    (!q || [s.title, s.description, s.note, s.branch, s.project].join(" ").toLowerCase().includes(q)));
}

/// Fetches and renders the list. A quiet load is the auto refresh: it keeps the
/// scroll position and focus, leaves the page as it was if the fetch fails, and
/// refreshes the open drawer's figures.
async function load(quiet) {
  if (quiet && loading) return;
  const seq = ++loadSeq;
  loading = true;
  let data;
  try {
    const r = await fetch("/api/sessions?period=" + encodeURIComponent(period) + "&automated=" + (automated ? "1" : "0"));
    if (!r.ok) throw new Error("status " + r.status);
    data = await r.json();
  } catch (_) {
    if (seq !== loadSeq) return;
    loading = false;
    if (!quiet) showFailure();
    else if ($("empty").textContent !== MSG_FAILED) $("stale").hidden = false;
    return;
  }
  if (seq !== loadSeq) return;
  loading = false;
  $("stale").hidden = true;
  sessions = Array.isArray(data.sessions) ? data.sessions : [];
  costMode = data.cost_mode || "list";
  costLabel = typeof data.cost_label === "string" && data.cost_label !== "" ? data.cost_label : COST_FALLBACK;
  demo = data.demo === true;
  $("demo").hidden = !demo;
  const basis = $("basis");
  basis.hidden = costMode !== "list";
  basis.textContent = LIST_BASIS + (typeof data.prices_as_of === "string" ? " Prices as of " + data.prices_as_of + "." : "");
  if (project !== null && !sessions.some(s => projectKey(s) === project)) project = null;
  const view = quiet ? saveView() : null;
  renderHeader();
  renderProjects();
  renderRows();
  if (view) restoreView(view);
  if (quiet && current !== null && sessions.some(s => s.id === current)) refreshDrawer(current);
}

/// What a re-render could disturb: the scroll position and focus on a row or project.
function saveView() {
  const a = document.activeElement;
  const row = a && a.closest && a.closest("tr.entry");
  const proj = a && a.closest && a.closest("#projects button");
  return { x: scrollX, y: scrollY, rail: $("projects").scrollLeft,
    row: row ? row.dataset.id : null, hasProj: !!proj, proj: proj ? buttonKey(proj) : null };
}

function restoreView(v) {
  let target = null;
  if (v.row !== null) target = [...document.querySelectorAll("tr.entry")].find(r => r.dataset.id === v.row);
  if (v.hasProj) target = [...$("projects").querySelectorAll("button")].find(b => buttonKey(b) === v.proj);
  if (target && document.activeElement !== target) target.focus({ preventScroll: true });
  $("projects").scrollLeft = v.rail;
  scrollTo(v.x, v.y);
}

function showFailure() {
  sessions = [];
  $("stats").replaceChildren();
  $("basis").hidden = true;
  $("stale").hidden = true;
  $("table").hidden = true;
  $("rows").replaceChildren();
  $("projects").replaceChildren();
  $("empty").textContent = MSG_FAILED;
}

function renderHeader() {
  const cols = [["", ""], ["c-where", ""], ["c-when", ""], ["c-tok", ""]];
  const heads = [["Session", ""], ["Folder and branch", "where"], ["Last active", ""], ["Tokens", "num tok"]];
  if (showCost()) { cols.push(["c-cost"]); heads.push([costLabel, "num"]); }
  $("cols").replaceChildren(...cols.map(([c]) => el("col", c)));
  $("thead").replaceChildren(...heads.map(([t, c]) => el("th", c, t)));
}

function renderProjects() {
  const counts = new Map();
  for (const s of sessions) counts.set(projectKey(s), (counts.get(projectKey(s)) || 0) + 1);
  const item = (label, key, n) => {
    const b = el("button");
    b.type = "button";
    b.setAttribute("aria-pressed", String(project === key));
    b.title = label;
    b.dataset.kind = key === null ? "all" : key === NO_FOLDER ? "none" : "label";
    b.dataset.key = typeof key === "string" ? key : "";
    b.append(el("span", "name", label), el("span", "n", n));
    const li = el("li");
    li.append(b);
    return li;
  };
  const items = [item("All projects", null, sessions.length)];
  for (const [key, n] of counts) items.push(item(projectName(key), key, n));
  $("projects").replaceChildren(...items);
  $("heading").textContent = project === null ? "Sessions" : projectName(project);
}

function renderStats(list) {
  const tiles = [["Sessions", list.length], ["Projects", new Set(list.map(projectKey)).size],
    ["Tokens", tokens(totalTokens(list))]];
  const nodes = tiles.map(([k, v]) => {
    const d = el("div", "stat");
    d.append(el("span", "", k), el("b", "", v));
    return d;
  });
  if (showCost()) {
    const d = el("div", "stat");
    d.append(el("span", "", costLabel), putCost(el("b"), costOf(list)));
    nodes.push(d);
  }
  $("stats").classList.toggle("three", !showCost());
  $("stats").replaceChildren(...nodes);
}

function renderRows() {
  const list = visible();
  renderStats(list);
  const body = [];
  let day = null;
  for (const s of list) {
    const at = lastActive(s);
    const key = at == null ? "none" : dayKey(at);
    if (key !== day) {
      day = key;
      const same = list.filter(x => { const t = lastActive(x); return (t == null ? "none" : dayKey(t)) === key; });
      const tr = el("tr", "day");
      const label = el("td", "", at == null ? "No date" : dayName(at));
      label.colSpan = narrow.matches ? 2 : 3; // the folder column is hidden when narrow
      tr.append(label, el("td", "num tok", tokens(totalTokens(same))));
      if (showCost()) tr.append(putCost(el("td", "num"), costOf(same)));
      body.push(tr);
    }
    body.push(entryRow(s));
  }
  $("rows").replaceChildren(...body);
  $("table").hidden = list.length === 0;
  $("empty").textContent = sessions.length === 0 ? (period === "all" ? MSG_EMPTY : MSG_EMPTY_PERIOD)
    : list.length === 0 ? MSG_NO_MATCH : "";
}

function entryRow(s) {
  const tr = el("tr", "entry");
  tr.tabIndex = 0;
  tr.setAttribute("role", "button");
  tr.dataset.id = s.id;
  if (s.id === current) tr.setAttribute("aria-current", "true");

  const what = el("td");
  what.append(el("span", "title", s.title));
  if (s.description) {
    what.append(el("span", "desc", (s.description_kind === "first_prompt" ? "Started with: " : "") + s.description));
  }

  const where = el("td", "where");
  where.title = s.project_dir || "";
  where.append(pathLine(shortDir(s.project_dir)));
  if (s.branch) where.append(el("span", "branch", "on " + s.branch));

  const when = el("td", "when");
  const at = lastActive(s);
  if (s.running) when.append(el("span", "live", "Running now"));
  else when.append(document.createTextNode(at == null ? "" : clock(at)));
  when.append(el("small", "", duration(s.active_ms)));

  tr.append(what, where, when, el("td", "num tok", tokens(s.tokens ? s.tokens.total : 0)));
  if (showCost()) tr.append(putCost(el("td", "num cost"), costOf([s])));
  return tr;
}

/// A folder on one line that keeps its last component in view: the part before it
/// shrinks with an ellipsis first. The "/" before the last component stays with it,
/// so a cut path reads "~/Documents/proje…/my-app".
function pathLine(dir) {
  const line = el("span", "path");
  const cut = dir.lastIndexOf("/");
  if (cut <= 0 || cut === dir.length - 1) {
    line.append(el("span", "leaf", dir)); // no parent to shorten: "", "/", "x", "/x", "x/"
  } else {
    line.append(el("span", "parent", dir.slice(0, cut)), el("span", "leaf", dir.slice(cut)));
  }
  return line;
}

// ---- drawer ----
function setCurrentRow(id) {
  for (const r of document.querySelectorAll("tr.entry")) {
    if (r.dataset.id === id) r.setAttribute("aria-current", "true");
    else r.removeAttribute("aria-current");
  }
}

async function openDrawer(id) {
  flushNote();
  if (!document.body.classList.contains("open")) {
    const row = document.activeElement && document.activeElement.closest && document.activeElement.closest("tr.entry");
    returnFocusId = row ? row.dataset.id : null;
  }
  current = id;
  setCurrentRow(id);
  let s = null;
  try {
    const r = await fetch("/api/sessions/" + encodeURIComponent(id));
    if (r.ok) s = await r.json();
  } catch (_) { /* falls through to the list row */ }
  if (current !== id) return;
  const fallback = sessions.find(x => x.id === id);
  renderDrawer(s || fallback || { id, title: "" }, !s);
  document.body.classList.add("open");
  $("close").focus();
}

/// Re-reads the session the drawer shows, for the auto refresh, and updates its
/// running state and figures; the note, the resume card and focus are left alone.
/// A drawer that was rendered after a failed load is rendered again in full.
async function refreshDrawer(id) {
  if (!shown || shown.id !== id) return; // mid-switch: the drawer still shows another session
  let s = null;
  try {
    const r = await fetch("/api/sessions/" + encodeURIComponent(id));
    if (r.ok) s = await r.json();
  } catch (_) { /* keeps what is shown */ }
  if (!s || current !== id || !shown || shown.id !== id || !document.body.classList.contains("open")) return;
  if (shown.failed) { renderDrawer(s, false, true); return; }
  renderSub(s);
  renderFigures(s, false);
  if (!$("d-facts").contains(document.activeElement)) renderFacts(s);
}

/// `keepScroll` is for a re-render of the same session, which leaves the drawer where it was.
function renderDrawer(s, failed, keepScroll) {
  shown = { id: s.id, failed };
  $("d-title").textContent = s.title || "";
  renderSub(s);
  renderResume(s, failed);

  $("d-desc").textContent = failed ? MSG_FAILED
    : s.description_kind === "recap" ? s.description : NO_RECAP;

  captureNote(); // typing during a row switch belongs to the session shown while typed
  const note = $("d-note");
  // In demo mode the real unsaved notes kept in this browser are neither shown nor touched.
  let pending = demo ? undefined : unsaved.get(s.id);
  // A note restored from an earlier visit is checked against the server's note once known.
  const serverKnown = !failed || sessions.some(x => x.id === s.id);
  if (pending && pending.restored && serverKnown) {
    const server = s.note || "";
    pending.restored = false;
    if (pending.text === server) { unsaved.delete(s.id); dropStored(s.id); pending = undefined; }
    else if (pending.base !== undefined && server !== pending.base) pending.conflict = true;
  }
  note.dataset.saved = s.note || "";
  note.dataset.id = s.id;
  note.readOnly = demo;
  note.placeholder = demo ? "Notes are hidden in demo mode" : NOTE_PLACEHOLDER;
  note.value = pending ? pending.text : note.dataset.saved;
  $("d-saved").textContent = !pending ? "" : pending.conflict ? NOTE_CONFLICT : pending.failed ? NOTE_FAILED : "";

  $("d-first").textContent = s.first_prompt || "";
  $("d-last").textContent = s.last_prompt || "";
  $("d-first-box").hidden = !s.first_prompt;
  $("d-last-box").hidden = !s.last_prompt;

  renderFigures(s, failed);
  renderFacts(s);
  if (!keepScroll) $("drawer").querySelector(".d-body").scrollTop = 0;
}

function renderSub(s) {
  const at = lastActive(s);
  const where = (s.project ? " in " + s.project : "") + (s.branch ? ", on " + s.branch : "");
  $("d-sub").textContent = s.running ? "Running now" + where
    : at == null ? where.trim() : dayName(at) + " at " + clock(at) + where;
}

/// The resume card is never blank: a command, or a line saying why there is none.
function renderResume(s, failed) {
  const canResume = !failed && !!(s.resumable && s.resume_command);
  $("d-can-resume").hidden = !canResume;
  $("d-cmd").textContent = canResume ? s.resume_command : "";
  $("d-dir-gone").hidden = !canResume || s.dir_exists !== false;
  $("copy").textContent = "Copy resume command";
  $("said").textContent = "";
  $("d-gone").hidden = canResume;
  $("d-gone").textContent = canResume ? "" : failed ? RESUME_FAILED
    : RESUME_BLOCKED.get(s.resume_blocked) || RESUME_BLOCKED.get("transcript_removed");
}

function renderFigures(s, failed) {
  const t = s.tokens || {};
  const figs = [["Input", t.input], ["Output", t.output], ["Cache read", t.cache_read],
    ["Cache write", (t.cache_write_5m || 0) + (t.cache_write_1h || 0)]].map(([k, v]) => {
    const tr = el("tr");
    tr.append(el("td", "", k), el("td", "", exact(v)));
    return tr;
  });
  const total = el("tr", "total");
  const costCell = el("td");
  if (showCost() && !failed) {
    const c = costOf([s]);
    putCost(costCell, c);
    if (!c.unknown) costCell.textContent = "about " + c.text;
  }
  total.append(el("td", "", exact(t.total) + " tokens"), costCell);
  $("d-fig").replaceChildren(...figs, total);
  renderSplit(failed ? null : s.split);
}

function renderFacts(s) {
  const started = s.started_at;
  const facts = [];
  const fact = (label, value, cls) => {
    if (value === undefined || value === null || value === "") return;
    const dd = el("dd", cls || "");
    if (value instanceof Node) dd.append(value); else dd.textContent = value;
    facts.push(el("dt", "", label), dd);
  };
  fact("Folder", shortDir(s.project_dir), "mono");
  fact("Branch", s.branch);
  if (started != null) fact("Started", dayName(started) + " at " + clock(started));
  fact("Duration", duration(s.active_ms));
  if (Array.isArray(s.models) && s.models.length) fact(s.models.length > 1 ? "Models" : "Model", s.models.join(", "));
  fact("Session ID", s.id, "mono");
  if (s.continued_from && s.continued_from.id) {
    const b = el("button", "link", s.continued_from.title || s.continued_from.id);
    b.type = "button";
    b.addEventListener("click", () => openDrawer(s.continued_from.id));
    fact("Continued from", b);
  }
  $("d-facts").replaceChildren(...facts);
}

/// One muted line under the figures: how the total splits between the main
/// conversation and its sub-agents. Hidden when no sub-agent used any tokens.
function renderSplit(split) {
  const line = $("d-split");
  line.removeAttribute("title");
  const main = split && split.main, subs = split && split.subagents;
  if (!main || !subs || !(subs.tokens > 0)) {
    line.textContent = "";
    line.hidden = true;
    return;
  }
  const n = subs.count || 0;
  const who = n === 1 ? "1 sub-agent" : n > 1 ? n + " sub-agents" : "Sub-agents";
  const cost = g => g.cost === null || g.cost === undefined ? "price unknown"
    : "about " + money(g.cost) + (g.cost_partial ? "+" : "");
  const part = (label, g) => label + " " + tokens(g.tokens) + " tokens" + (showCost() ? ", " + cost(g) : "") + ".";
  line.textContent = part("Main conversation", main) + " " + part(who, subs);
  if (showCost() && (main.cost_partial || subs.cost_partial)) line.title = PARTIAL_TITLE;
  line.hidden = false;
}

function closeDrawer() {
  if (!document.body.classList.contains("open")) return;
  flushNote();
  document.body.classList.remove("open");
  current = null;
  shown = null;
  setCurrentRow(null);
  const row = returnFocusId && [...document.querySelectorAll("tr.entry")].find(r => r.dataset.id === returnFocusId);
  if (row) row.focus();
  returnFocusId = null;
}

// ---- note ----
let noteTimer = null;
const NOTE_FAILED = "Could not save. Try again.";
// Note text not yet confirmed by the server, by session id:
//   { text, base, failed, restored, conflict }
// `base` is the server's note when the user started editing. `restored` marks an
// entry loaded from storage and not yet checked against the server's note;
// `conflict` means the server's note changed elsewhere since `base`. Neither kind
// is saved again until the user edits it.
// An entry stays until a PUT of that exact text succeeds, so switching rows,
// closing the drawer or a failed save never drops what was typed.
const unsaved = new Map();
const saveSeq = new Map(); // latest PUT number per session; older responses don't set status

// Unsaved text is also kept in localStorage, so it survives leaving the page.
// Storage may be missing or throw (blocked site data, private windows); the
// in-memory map then carries on alone.
const STORE_PREFIX = "sled.unsaved.";
// Stored as JSON { v: 2, text, base }; an entry from before `base` existed is the bare text.
function storeUnsaved(id, text, base) {
  try { localStorage.setItem(STORE_PREFIX + id, JSON.stringify({ v: 2, text, base })); } catch (_) { /* kept in memory only */ }
}
function parseStored(raw) {
  try {
    const v = JSON.parse(raw);
    if (v && v.v === 2 && typeof v.text === "string") {
      return { text: v.text, base: typeof v.base === "string" ? v.base : undefined };
    }
  } catch (_) { /* bare text */ }
  return { text: raw, base: undefined };
}
function dropStored(id) {
  try { localStorage.removeItem(STORE_PREFIX + id); } catch (_) { /* nothing to remove */ }
}
/// Loads notes left unsaved by an earlier visit. renderDrawer checks each against the
/// server's note when its session is opened.
function restoreUnsaved() {
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (!key || !key.startsWith(STORE_PREFIX) || key.length === STORE_PREFIX.length) continue;
      const raw = localStorage.getItem(key);
      if (typeof raw !== "string") continue;
      const { text, base } = parseStored(raw);
      unsaved.set(key.slice(STORE_PREFIX.length), { text, base, failed: true, restored: true });
    }
  } catch (_) { /* storage unavailable */ }
}

/// Saves the note box's text to the session it belongs to if it changed since it
/// was last saved or queued; unlike flushNote it does not retry an unchanged entry.
function captureNote() {
  if (noteTimer) { clearTimeout(noteTimer); noteTimer = null; }
  if (demo) return;
  const note = $("d-note");
  const id = note.dataset.id;
  if (!id) return;
  const pending = unsaved.get(id);
  if (note.value !== (pending ? pending.text : note.dataset.saved)) saveNote(id, note.value);
}

function flushNote() {
  if (noteTimer) { clearTimeout(noteTimer); noteTimer = null; }
  if (demo) return;
  const note = $("d-note");
  const id = note.dataset.id;
  if (!id) return;
  const entry = unsaved.get(id);
  if (entry && (entry.restored || entry.conflict) && note.value === entry.text) return; // waits for an edit
  if (note.value !== note.dataset.saved || entry) saveNote(id, note.value);
}

async function saveNote(id, text) {
  if (demo) return;
  const prev = unsaved.get(id);
  const note = $("d-note");
  const base = prev && prev.base !== undefined ? prev.base
    : note.dataset.id === id ? note.dataset.saved : undefined;
  unsaved.set(id, { text, base, failed: false });
  storeUnsaved(id, text, base);
  const seq = (saveSeq.get(id) || 0) + 1;
  saveSeq.set(id, seq);
  let ok = false;
  try {
    const r = await fetch("/api/sessions/" + encodeURIComponent(id) + "/note", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ note: text }),
      keepalive: true,
    });
    ok = r.ok;
  } catch (_) { ok = false; }
  const entry = unsaved.get(id);
  const latest = saveSeq.get(id) === seq;
  if (ok) {
    const s = sessions.find(x => x.id === id);
    if (s) s.note = text;
    if (entry && entry.text === text) { unsaved.delete(id); dropStored(id); }
  } else if (latest && entry && entry.text === text) {
    entry.failed = true;
  }
  // Status belongs to the drawer only while it shows this session.
  if (!latest || note.dataset.id !== id || !document.body.classList.contains("open")) return;
  if (ok) note.dataset.saved = text;
  $("d-saved").textContent = ok ? "Saved" : NOTE_FAILED;
}

$("d-note").addEventListener("input", () => {
  const note = $("d-note");
  const entry = unsaved.get(note.dataset.id);
  if (entry && (entry.restored || entry.conflict)) {
    // Editing claims the text: it is saved over the server's current note.
    entry.restored = entry.conflict = false;
    entry.base = note.dataset.saved;
  }
  if (noteTimer) clearTimeout(noteTimer);
  $("d-saved").textContent = "";
  noteTimer = setTimeout(flushNote, 600);
});
$("d-note").addEventListener("blur", flushNote);

// ---- copy ----
$("copy").addEventListener("click", async () => {
  const cmd = $("d-cmd").textContent;
  let ok = false;
  try { await navigator.clipboard.writeText(cmd); ok = true; } catch (_) {
    const t = el("textarea");
    t.value = cmd;
    t.setAttribute("readonly", "");
    t.style.position = "fixed"; t.style.opacity = "0";
    document.body.append(t);
    t.select();
    try { ok = document.execCommand("copy"); } catch (_) { ok = false; }
    t.remove();
    $("copy").focus();
  }
  if (ok) {
    $("copy").textContent = "Copied";
    $("said").textContent = "Paste it into any terminal.";
  } else {
    getSelection().selectAllChildren($("d-cmd"));
    $("said").textContent = COPY_FAILED;
  }
});

// ---- events ----
$("rows").addEventListener("click", e => {
  const r = e.target.closest("tr.entry");
  if (r) { r.focus(); openDrawer(r.dataset.id); }
});
$("rows").addEventListener("keydown", e => {
  if (e.key !== "Enter" && e.key !== " ") return;
  const r = e.target.closest("tr.entry");
  if (r) { e.preventDefault(); openDrawer(r.dataset.id); }
});
$("projects").addEventListener("click", e => {
  const b = e.target.closest("button");
  if (!b) return;
  project = buttonKey(b);
  renderProjects();
  renderRows();
});
$("search").addEventListener("input", e => { query = e.target.value; renderRows(); });
$("range").addEventListener("click", e => {
  const b = e.target.closest("button");
  if (!b || b.dataset.period === period) return;
  period = b.dataset.period;
  for (const x of $("range").querySelectorAll("button")) x.setAttribute("aria-pressed", String(x === b));
  load();
});
$("automated").addEventListener("change", e => { automated = e.target.checked; load(); });
$("close").addEventListener("click", closeDrawer);
$("scrim").addEventListener("click", closeDrawer);
document.addEventListener("keydown", e => {
  if (!document.body.classList.contains("open")) return;
  if (e.key === "Escape") { e.preventDefault(); closeDrawer(); return; }
  if (e.key !== "Tab") return;
  // Keep focus inside the open drawer.
  const focusable = [...$("drawer").querySelectorAll("button, textarea")].filter(x => x.offsetParent !== null);
  if (!focusable.length) return;
  const first = focusable[0], last = focusable[focusable.length - 1];
  if (!$("drawer").contains(document.activeElement)) { e.preventDefault(); first.focus(); }
  else if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
  else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
});
window.addEventListener("pagehide", flushNote);
narrow.addEventListener("change", () => { if (!$("table").hidden) renderRows(); });

// ---- auto refresh: every 30 s while the tab is visible, once on becoming visible ----
let refreshTimer = null;
function scheduleRefresh() {
  clearTimeout(refreshTimer);
  refreshTimer = document.hidden ? null : setTimeout(refresh, REFRESH_MS);
}
function refresh() {
  scheduleRefresh();
  load(true);
}
document.addEventListener("visibilitychange", () => { if (document.hidden) scheduleRefresh(); else refresh(); });

restoreUnsaved();
load();
scheduleRefresh();
