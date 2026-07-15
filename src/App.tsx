import { FormEvent, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { marked } from "marked";
import DOMPurify from "dompurify";
import "./App.css";
import claudeLogo from "./assets/providers/claude.svg";
import codexLogo from "./assets/providers/codex.svg";
import geminiLogo from "./assets/providers/gemini.svg";

interface SessionView {
  id: string;
  session_id: string | null;
  name: string;
  agent: string;
  cwd: string;
  status: string;
  source: string;
  pane_id: string | null;
  pid: number | null;
  transcript_path: string | null;
  updated_at: number | null;
  workspace: string | null;
  workspace_id: string | null;
  workspace_order: number | null;
  tab_id: string | null;
  tab_label: string | null;
  tab_order: number | null;
  display_name: string;
  branch: string | null;
}

interface TimelineItem {
  role: string;
  text: string;
  ts: string | null;
  tool_name: string | null;
}

/** Model/effort ride along with the transcript because that's the only place they're recorded —
 *  and only where the CLI writes them: Claude logs no effort, Gemini logs neither. */
interface Timeline {
  items: TimelineItem[];
  model: string | null;
  effort: string | null;
}

interface ProviderInstallation {
  provider: string;
  installed: boolean;
  version: string | null;
}

interface HerdrEventStatus {
  connected: boolean;
  error: string | null;
}

const providerLabels: Record<string, string> = {
  claude: "Claude Code",
  codex: "Codex CLI",
  gemini: "Gemini CLI",
};

const providerMarks: Record<string, string> = {
  claude: "C",
  codex: "◌",
  gemini: "G",
  other: "A",
};

// Brand SVGs on their app-icon chip: Claude = white burst on terracotta, OpenAI = white
// blossom on black, Gemini = 4-color spark. Rendered as plain images (consistent light/dark).
const providerLogos: Record<string, string> = {
  claude: claudeLogo,
  codex: codexLogo,
  gemini: geminiLogo,
};

function ProviderLogo({ provider }: { provider: string }) {
  const src = providerLogos[provider];
  if (src) return <img className="provider-logo" src={src} alt="" />;
  return <>{providerMarks[provider] ?? providerMarks.other}</>;
}

type SpaceTab = { key: string; label: string; number: number; sessionId: string; count: number };

/** A session's key in the tab strip: its herdr tab, or itself when herdr knows no tab for it.
 *  Keyed by tab_id, not by label — herdr labels repeat ("1", "2") across tabs. */
function tabKeyOf(session: SessionView) {
  return session.tab_id ?? `__${session.id}`;
}

/** The tabs of a session's space in herdr's own tab order (tab_order), one entry per tab,
 *  carrying a representative session to switch to and how many panes it holds. */
function spaceTabsOf(sessions: SessionView[], selected: SessionView): SpaceTab[] {
  if (!selected.workspace_id) return [];
  const byTab = new Map<string, SpaceTab>();
  for (const session of sessions) {
    if (session.workspace_id !== selected.workspace_id) continue;
    const key = tabKeyOf(session);
    const entry = byTab.get(key);
    if (entry) entry.count += 1;
    else
      byTab.set(key, {
        key,
        label: session.tab_label ?? "탭",
        number: session.tab_order ?? Number.MAX_SAFE_INTEGER,
        sessionId: session.id,
        count: 1,
      });
  }
  return [...byTab.values()].sort((a, b) => a.number - b.number);
}

/** herdr shows a workspace's tabs as a strip along the top; we do the same, but only when a
 *  space actually has more than one tab. Clicking a tab jumps to a session inside it. */
function SpaceTabs({
  sessions,
  selected,
  onSelect,
}: {
  sessions: SessionView[];
  selected: SessionView;
  onSelect: (id: string) => void;
}) {
  const tabs = spaceTabsOf(sessions, selected);
  if (tabs.length < 2) return null;
  const activeKey = tabKeyOf(selected);
  return (
    <div className="space-tabs" role="tablist" aria-label={`${selected.workspace ?? "space"} 탭`}>
      {tabs.map((tab) => {
        const active = tab.key === activeKey;
        return (
          <button
            key={tab.key}
            type="button"
            role="tab"
            aria-selected={active}
            className={`space-tab ${active ? "active" : ""}`}
            onClick={() => onSelect(tab.sessionId)}
          >
            <span className="space-tab-label">{tab.label}</span>
            {tab.count > 1 && <span className="space-tab-count">{tab.count}</span>}
          </button>
        );
      })}
    </div>
  );
}

// herdr's status vocabulary, ported 1:1 from src/ui/status.rs. The backend now passes herdr's
// semantic state through, so blocked/working/done/idle/unknown all arrive verbatim.
type HerdrState = "blocked" | "working" | "done" | "idle" | "unknown";

// The braille spinner herdr animates for a working agent (src/ui.rs spinner_frame).
const SPINNER_FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// agent_icon(): the leading glyph on an agent row. Only "working" animates.
function agentIcon(state: HerdrState, tick: number): string {
  switch (state) {
    case "blocked":
      return "◉";
    case "working":
      return SPINNER_FRAMES[tick % SPINNER_FRAMES.length];
    case "done":
      return "●";
    case "idle":
      return "✓";
    default:
      return "○";
  }
}

// state_dot(): the rolled-up dot herdr paints on a space (our group header).
function stateDot(state: HerdrState): string {
  if (state === "idle") return "○";
  if (state === "unknown") return "·";
  return "●";
}

const stateLabels: Record<HerdrState, string> = {
  blocked: "blocked",
  working: "working",
  done: "done",
  idle: "idle",
  unknown: "idle",
};

const stateColorClass: Record<HerdrState, string> = {
  blocked: "st-red",
  working: "st-yellow",
  done: "st-teal",
  idle: "st-green",
  unknown: "st-gray",
};

// herdr rolls a space up to its most urgent agent: blocked > working > done > idle.
const STATE_RANK: Record<HerdrState, number> = {
  blocked: 0,
  working: 1,
  done: 2,
  idle: 3,
  unknown: 4,
};

function rollUpState(states: HerdrState[]): HerdrState {
  return states.reduce<HerdrState>(
    (worst, state) => (STATE_RANK[state] < STATE_RANK[worst] ? state : worst),
    "idle",
  );
}

// The backend hands us herdr's state string directly; normalize anything unexpected to unknown.
function asState(status: string): HerdrState {
  return status === "blocked" || status === "working" || status === "done" || status === "idle"
    ? status
    : "unknown";
}

const COLLAPSED_MESSAGE_HEIGHT = 190;

function providerOf(session: SessionView) {
  return session.agent in providerMarks ? session.agent : "other";
}

function providerLabelOf(session: SessionView) {
  return providerLabels[session.agent] ?? session.agent;
}

function shortPath(path: string) {
  const parts = path.split("/").filter(Boolean);
  return parts.length > 2 ? `…/${parts.slice(-2).join("/")}` : path;
}

function formatEventTime(timestamp: string | null) {
  if (!timestamp) return "";
  return new Intl.DateTimeFormat("ko-KR", { hour: "2-digit", minute: "2-digit" }).format(new Date(timestamp));
}

function relativeTime(updatedAt: number | null) {
  if (!updatedAt) return "";
  const seconds = Math.max(0, Math.floor((Date.now() - updatedAt) / 1000));
  if (seconds < 60) return "방금";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}분 전`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)}시간 전`;
  return `${Math.floor(seconds / 86_400)}일 전`;
}

function workspaceDropTargetAt(clientX: number, clientY: number) {
  const element = document.elementFromPoint(clientX, clientY);
  const group = element?.closest<HTMLElement>("[data-workspace-key]");
  const key = group?.dataset.workspaceKey;
  if (!group || !key) return null;
  const header = group.querySelector<HTMLElement>(".group-header-row");
  const bounds = (header ?? group).getBoundingClientRect();
  return { key, after: clientY > bounds.top + bounds.height / 2 };
}

/** Groups sessions by herdr workspace; pinned groups stay in the top band while the
 *  user's persisted drag order decides the order within each band. */
function groupSessions(sessions: SessionView[], pinned: string[], workspaceOrder: string[]) {
  const groups = new Map<string, { label: string; order: number; sessions: SessionView[] }>();
  for (const session of sessions) {
    const key = session.workspace_id ?? "__other__";
    if (!groups.has(key)) {
      groups.set(key, {
        label: session.workspace ?? "기타",
        order: session.workspace_order ?? Number.MAX_SAFE_INTEGER,
        sessions: [],
      });
    }
    groups.get(key)!.sessions.push(session);
  }
  // Inside a group, follow herdr's tab order; panes of the same tab keep backend order (stable sort).
  for (const group of groups.values()) {
    group.sessions.sort(
      (a, b) => (a.tab_order ?? Number.MAX_SAFE_INTEGER) - (b.tab_order ?? Number.MAX_SAFE_INTEGER),
    );
  }
  return [...groups.entries()]
    .map(([key, group]) => ({ key, ...group, pinned: pinned.includes(key) }))
    .sort((a, b) => {
      const pinnedOrder = Number(b.pinned) - Number(a.pinned);
      if (pinnedOrder) return pinnedOrder;
      const aManual = workspaceOrder.indexOf(a.key);
      const bManual = workspaceOrder.indexOf(b.key);
      if (aManual >= 0 || bManual >= 0) {
        if (aManual < 0) return 1;
        if (bManual < 0) return -1;
        if (aManual !== bManual) return aManual - bManual;
      }
      return a.order - b.order;
    });
}

/** Pin/collapse choices are per-machine UI state, so localStorage is the whole store. */
function useStoredKeys(storageKey: string) {
  const [keys, setKeys] = useState<string[]>(() => {
    try {
      const raw = localStorage.getItem(storageKey);
      const parsed = raw ? JSON.parse(raw) : [];
      return Array.isArray(parsed) ? parsed.filter((k) => typeof k === "string") : [];
    } catch {
      return [];
    }
  });
  const update = useCallback(
    (updater: (current: string[]) => string[]) =>
      setKeys((current) => {
        const next = updater(current);
        localStorage.setItem(storageKey, JSON.stringify(next));
        return next;
      }),
    [storageKey],
  );
  const toggle = useCallback(
    (key: string) => update((current) => (
      current.includes(key) ? current.filter((item) => item !== key) : [...current, key]
    )),
    [update],
  );
  return [keys, toggle, update] as const;
}

let mermaidSeq = 0;
let mermaidQueue: Promise<void> = Promise.resolve();
const mermaidCache = new Map<string, Promise<string>>();

/** Mermaid has global configuration, so serialize light/dark renders and cache their sanitized SVG.
 *  Most importantly, the SVG is now part of React's HTML value instead of an imperative DOM patch;
 *  a transcript refresh can no longer restore the original ```mermaid fence behind React's back. */
function mermaidSvg(source: string, theme: "light" | "dark") {
  const cacheKey = `${theme}\u0000${source}`;
  const cached = mermaidCache.get(cacheKey);
  if (cached) return cached;

  const task = mermaidQueue.then(async () => {
    const { default: mermaid } = await import("mermaid");
    // htmlLabels:false keeps labels in SVG <text>; foreignObject would reopen an HTML/mXSS
    // integration point for transcript content that must be treated as adversarial.
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: "strict",
      htmlLabels: false,
      flowchart: { htmlLabels: false },
      theme: theme === "dark" ? "dark" : "default",
    });
    const id = `mmd-${mermaidSeq++}`;
    try {
      const { svg } = await mermaid.render(id, source);
      return DOMPurify.sanitize(svg);
    } catch (error) {
      document.getElementById(id)?.remove();
      throw error;
    }
  });

  mermaidQueue = task.then(() => undefined, () => undefined);
  mermaidCache.set(cacheKey, task);
  void task.then(
    () => {
      // Diagrams recur frequently in a transcript, but an unlimited process-lifetime cache does not.
      while (mermaidCache.size > 100) mermaidCache.delete(mermaidCache.keys().next().value!);
    },
    () => {
      if (mermaidCache.get(cacheKey) === task) mermaidCache.delete(cacheKey);
    },
  );
  return task;
}

function markdownTemplate(content: string) {
  const template = document.createElement("template");
  template.innerHTML = DOMPurify.sanitize(marked.parse(content, { async: false }) as string);
  return template;
}

/** Give fenced content Codex-like, neutral chrome and wrapping instead of a terminal-black slab. */
function decorateCodeBlocks(root: DocumentFragment) {
  for (const pre of [...root.querySelectorAll("pre")]) {
    if (pre.closest(".code-block")) continue;
    const code = pre.querySelector(":scope > code");
    if (!code) continue;
    const languageClass = [...code.classList].find((name) => name.startsWith("language-"));
    const language = languageClass?.slice("language-".length) || "text";
    const figure = document.createElement("figure");
    figure.className = `code-block ${language === "text" || language === "plaintext" ? "plain" : ""}`.trim();
    const caption = document.createElement("figcaption");
    const label = document.createElement("span");
    label.textContent = language;
    const copy = document.createElement("button");
    copy.type = "button";
    copy.dataset.copyCode = "";
    copy.textContent = "복사";
    caption.append(label, copy);
    pre.replaceWith(figure);
    figure.append(caption, pre);
  }
}

function initialMarkdownHtml(content: string) {
  const template = markdownTemplate(content);
  for (const pre of [...template.content.querySelectorAll("pre")]) {
    if (!pre.querySelector(":scope > code.language-mermaid")) continue;
    const pending = document.createElement("div");
    pending.className = "mermaid-figure pending";
    pending.setAttribute("role", "status");
    pending.textContent = "차트 그리는 중…";
    pre.replaceWith(pending);
  }
  decorateCodeBlocks(template.content);
  return template.innerHTML;
}

async function renderedMarkdownHtml(content: string, theme: "light" | "dark") {
  const template = markdownTemplate(content);
  const mermaidBlocks = [...template.content.querySelectorAll("pre")]
    .map((pre) => ({ pre, code: pre.querySelector(":scope > code.language-mermaid") }))
    .filter((block): block is { pre: HTMLPreElement; code: HTMLElement } => Boolean(block.code));

  await Promise.all(mermaidBlocks.map(async ({ pre, code }) => {
    try {
      const figure = document.createElement("div");
      figure.className = "mermaid-figure";
      figure.innerHTML = await mermaidSvg(code.textContent ?? "", theme);
      pre.replaceWith(figure);
    } catch {
      // Invalid Mermaid remains available as an ordinary, readable source block.
    }
  }));
  decorateCodeBlocks(template.content);
  return template.innerHTML;
}

function MarkdownMessage({ content, theme, collapsible = false }: { content: string; theme: "light" | "dark"; collapsible?: boolean }) {
  const contentRef = useRef<HTMLDivElement | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [canCollapse, setCanCollapse] = useState(false);
  const initialHtml = useMemo(() => initialMarkdownHtml(content), [content]);
  const htmlKey = `${theme}\u0000${content}`;
  const [renderedHtml, setRenderedHtml] = useState({ key: htmlKey, html: initialHtml });
  const html = renderedHtml.key === htmlKey ? renderedHtml.html : initialHtml;

  useLayoutEffect(() => {
    const element = contentRef.current;
    if (!element || !collapsible) {
      setCanCollapse(false);
      return;
    }
    const measure = () => setCanCollapse(element.scrollHeight > COLLAPSED_MESSAGE_HEIGHT);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, [collapsible, content]);

  useEffect(() => {
    let cancelled = false;
    void renderedMarkdownHtml(content, theme).then((rendered) => {
      if (!cancelled) setRenderedHtml({ key: htmlKey, html: rendered });
    });
    return () => {
      cancelled = true;
    };
  }, [content, htmlKey, theme]);

  const copyCode = useCallback((event: React.MouseEvent<HTMLDivElement>) => {
    const button = (event.target as Element).closest<HTMLButtonElement>("button[data-copy-code]");
    if (!button) return;
    const source = button.closest(".code-block")?.querySelector("pre code")?.textContent;
    if (source == null) return;
    void navigator.clipboard.writeText(source).then(() => {
      button.textContent = "복사됨";
      window.setTimeout(() => {
        if (button.isConnected) button.textContent = "복사";
      }, 1_400);
    }).catch(() => {
      button.textContent = "복사 실패";
    });
  }, []);

  return (
    <>
      <div className={`message-content ${canCollapse && !expanded ? "collapsed" : ""}`} ref={contentRef} onClick={copyCode}>
        <div className="markdown-body" dangerouslySetInnerHTML={{ __html: html }} />
      </div>
      {canCollapse && (
        <button
          type="button"
          className="message-expand-button"
          aria-expanded={expanded}
          onClick={() => setExpanded((current) => !current)}
        >
          {expanded ? "접기" : "전체 메시지 펼치기"}<span aria-hidden="true">{expanded ? "⌃" : "⌄"}</span>
        </button>
      )}
    </>
  );
}

type Choice = { key: string; label: string; active: boolean };

// All three agents draw a blocked prompt as a numbered menu, each with its own cursor glyph and
// box chrome: Claude "❯ 1. Yes", Codex "› 1. Yes, proceed (y)", Gemini "│ ● 1. Allow once".
const CHOICE_LINE = /^[\s│]*([❯›●▶])?\s*([1-9])\.\s+(\S.*?)\s*│?\s*$/;

// Last numbered run wins — the live prompt sits at the bottom, above only dead scrollback.
function parseChoices(screen: string | null): Choice[] {
  if (!screen) return [];
  let run: Choice[] = [];
  let menu: Choice[] = [];
  for (const line of screen.split("\n")) {
    const hit = CHOICE_LINE.exec(line);
    if (!hit) continue;
    const number = Number(hit[2]);
    const choice = { key: hit[2], label: hit[3], active: Boolean(hit[1]) };
    if (number === run.length + 1) run.push(choice);
    else if (number === 1) run = [choice];
    else continue;
    if (run.length >= 2) menu = run;
  }
  return menu;
}

function App() {
  const [sessions, setSessions] = useState<SessionView[] | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [providers, setProviders] = useState<ProviderInstallation[]>([]);
  const [output, setOutput] = useState<string | null>(null);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [connectionError, setConnectionError] = useState<string | null>(null);
  const [outputError, setOutputError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [socketConnected, setSocketConnected] = useState<boolean | null>(null);
  const [timeline, setTimeline] = useState<Timeline | null>(null);
  const [viewMode, setViewMode] = useState<"chat" | "terminal">("chat");
  const [mobileSidebarOpen, setMobileSidebarOpen] = useState(false);
  const [pinned, togglePinned, updatePinned] = useStoredKeys("rl.pinnedWorkspaces");
  const [collapsed, toggleCollapsed] = useStoredKeys("rl.collapsedWorkspaces");
  const [workspaceOrder, , updateWorkspaceOrder] = useStoredKeys("rl.workspaceOrder");
  const [dragTarget, setDragTarget] = useState<{ key: string; after: boolean } | null>(null);
  const [theme, setTheme] = useState<"light" | "dark">(() => {
    const saved = localStorage.getItem("rl.theme");
    if (saved === "light" || saved === "dark") return saved;
    return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  });
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [spinnerTick, setSpinnerTick] = useState(0);
  const [listScrolling, setListScrolling] = useState(false);
  const scrollIdleTimer = useRef<number | null>(null);
  const listRef = useRef<HTMLElement | null>(null);
  const [offscreenBlocked, setOffscreenBlocked] = useState({ above: 0, below: 0 });
  const fleetRefreshTimer = useRef<number | null>(null);
  const fleetRequestRef = useRef(0);
  const revisionRef = useRef<number>(0);
  const conversationRef = useRef<HTMLDivElement | null>(null);
  const terminalRef = useRef<HTMLPreElement | null>(null);
  const pinnedToBottomRef = useRef({ chat: true, terminal: true });
  const selectedIdRef = useRef<string | null>(null);
  const pointerDragRef = useRef<{ key: string; pointerId: number; startY: number; active: boolean } | null>(null);
  const suppressWorkspaceClickRef = useRef(false);
  selectedIdRef.current = selectedId;

  const message = selectedId ? drafts[selectedId] ?? "" : "";

  // The dot carries the state now, so the wording it replaced lives on as its tooltip and label.
  const herdrStatus = connectionError
    ? { tone: "bad", label: "herdr 연결 끊김" }
    : socketConnected
      ? { tone: "live", label: "herdr · 실시간" }
      : { tone: "polling", label: "herdr · 폴링 모드" };

  // Theme lives on <html data-theme> so every CSS token flips at once; persisted per machine.
  useLayoutEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    localStorage.setItem("rl.theme", theme);
  }, [theme]);

  const refreshFleet = useCallback(async () => {
    const requestId = ++fleetRequestRef.current;
    try {
      const next = await invoke<SessionView[]>("list_sessions");
      if (requestId !== fleetRequestRef.current) return;
      setSessions(next);
      setConnectionError(null);
      setSelectedId((current) => {
        if (current && next.some((session) => session.id === current)) return current;
        return next[0]?.id ?? null;
      });
    } catch (error) {
      if (requestId !== fleetRequestRef.current) return;
      setConnectionError(String(error));
    }
  }, []);

  useEffect(() => {
    invoke<ProviderInstallation[]>("detect_providers").then(setProviders).catch(() => setProviders([]));
    void refreshFleet();
    // The socket pushes changes; this poll is only a safety net if it dies quietly.
    const interval = window.setInterval(() => void refreshFleet(), socketConnected ? 15_000 : 2_000);
    return () => window.clearInterval(interval);
  }, [refreshFleet, socketConnected]);

  useEffect(() => {
    const refreshStatus = () => {
      void invoke<HerdrEventStatus>("herdr_event_status")
        .then((status) => setSocketConnected(status.connected))
        .catch(() => setSocketConnected(false));
    };
    refreshStatus();
    const interval = window.setInterval(refreshStatus, 2_000);
    return () => window.clearInterval(interval);
  }, []);

  const selected = useMemo(
    () => sessions?.find((session) => session.id === selectedId) ?? null,
    [sessions, selectedId],
  );
  const groups = useMemo(
    () => groupSessions(sessions ?? [], pinned, workspaceOrder),
    [sessions, pinned, workspaceOrder],
  );
  const hasWorking = useMemo(
    () => (sessions ?? []).some((session) => session.status === "working"),
    [sessions],
  );

  // blocked is the one state the user must act on, so it must never hide: markers carry how many
  // blocked agents they stand for (a session row 1, a collapsed group all the ones it swallows),
  // and anything scrolled out of the list turns into a peek pill at that edge.
  const blockedMarkers = useCallback((direction?: "above" | "below") => {
    const list = listRef.current;
    if (!list) return [] as HTMLElement[];
    const bounds = list.getBoundingClientRect();
    return [...list.querySelectorAll<HTMLElement>("[data-blocked]")].filter((marker) => {
      const rect = marker.getBoundingClientRect();
      if (direction === "above") return rect.bottom < bounds.top + 4;
      if (direction === "below") return rect.top > bounds.bottom - 4;
      return true;
    });
  }, []);

  const countBlocked = (markers: HTMLElement[]) =>
    markers.reduce((sum, marker) => sum + (Number(marker.dataset.blocked) || 0), 0);

  const syncOffscreenBlocked = useCallback(() => {
    const above = countBlocked(blockedMarkers("above"));
    const below = countBlocked(blockedMarkers("below"));
    setOffscreenBlocked((prev) =>
      prev.above === above && prev.below === below ? prev : { above, below },
    );
  }, [blockedMarkers]);

  useEffect(() => {
    syncOffscreenBlocked();
    window.addEventListener("resize", syncOffscreenBlocked);
    return () => window.removeEventListener("resize", syncOffscreenBlocked);
  }, [syncOffscreenBlocked, groups, collapsed]);

  const scrollToBlocked = useCallback(
    (direction: "above" | "below") => {
      const hits = blockedMarkers(direction);
      const target = direction === "above" ? hits[hits.length - 1] : hits[0];
      target?.scrollIntoView({ block: "center", behavior: "smooth" });
    },
    [blockedMarkers],
  );

  // Advance the braille spinner only while something is actually working.
  useEffect(() => {
    if (!hasWorking) return;
    const timer = window.setInterval(
      () => setSpinnerTick((tick) => (tick + 1) % SPINNER_FRAMES.length),
      90,
    );
    return () => window.clearInterval(timer);
  }, [hasWorking]);

  const moveWorkspace = useCallback(
    (targetKey: string, after: boolean) => {
      const draggedKey = pointerDragRef.current?.key;
      if (!draggedKey || draggedKey === targetKey) return;

      const visibleOrder = groups.map((group) => group.key).filter((key) => key !== draggedKey);
      const targetIndex = visibleOrder.indexOf(targetKey);
      visibleOrder.splice(targetIndex + (after ? 1 : 0), 0, draggedKey);
      updateWorkspaceOrder(() => visibleOrder);

      // Crossing the pinned boundary should behave exactly like the visible drop:
      // dragging into the pinned band pins the group, and dragging out unpins it.
      const targetIsPinned = pinned.includes(targetKey);
      updatePinned((current) => {
        const withoutDragged = current.filter((key) => key !== draggedKey);
        return targetIsPinned ? [...withoutDragged, draggedKey] : withoutDragged;
      });
    },
    [groups, pinned, updatePinned, updateWorkspaceOrder],
  );

  const scheduleFleetRefresh = useCallback(() => {
    if (fleetRefreshTimer.current !== null) return;
    fleetRefreshTimer.current = window.setTimeout(() => {
      fleetRefreshTimer.current = null;
      void refreshFleet();
    }, 180);
  }, [refreshFleet]);

  const refreshOutput = useCallback(async () => {
    const sessionId = selected?.id ?? null;
    const paneId = selected?.pane_id ?? null;
    if (!sessionId || !paneId) {
      setOutput(null);
      return;
    }
    try {
      const next = await invoke<string>("read_pane", { paneId });
      if (selectedIdRef.current !== sessionId) return;
      setOutput(next);
      setOutputError(null);
    } catch (error) {
      if (selectedIdRef.current !== sessionId) return;
      setOutputError(String(error));
    }
  }, [selected?.id, selected?.pane_id]);

  const refreshTimeline = useCallback(async () => {
    const sessionId = selected?.id ?? null;
    const path = selected?.transcript_path;
    if (!sessionId || !path) {
      setTimeline(null);
      return;
    }
    try {
      const next = await invoke<Timeline>("get_timeline", { transcriptPath: path, last: 200 });
      if (selectedIdRef.current !== sessionId) return;
      setTimeline(next);
    } catch {
      if (selectedIdRef.current !== sessionId) return;
      setTimeline(null);
    }
  }, [selected?.id, selected?.transcript_path]);

  // A session with no transcript can only be watched as a terminal.
  useEffect(() => {
    setTimeline(null);
    setOutput(null);
    revisionRef.current = 0;
    setViewMode(selected?.transcript_path ? "chat" : "terminal");
    void refreshTimeline();
  }, [refreshTimeline, selected?.id]);

  // Chat view doesn't need the pane text — except while blocked, where the prompt only
  // exists on screen. herdr reports the state; the choices are nowhere but the terminal.
  const blocked = selected ? asState(selected.status) === "blocked" : false;

  useEffect(() => {
    if (viewMode !== "terminal" && !blocked) return;
    void refreshOutput();
    const interval = window.setInterval(() => void refreshOutput(), 2_000);
    return () => window.clearInterval(interval);
  }, [blocked, refreshOutput, viewMode]);

  const choices = useMemo(() => (blocked ? parseChoices(output) : []), [blocked, output]);

  const sendKeys = useCallback(
    async (keys: string[]) => {
      const targetSessionId = selected?.id;
      const targetPaneId = selected?.pane_id;
      if (!targetSessionId || !targetPaneId || sending) return;
      setSending(true);
      try {
        await invoke("send_keys", { paneId: targetPaneId, keys });
        if (selectedIdRef.current === targetSessionId) setOutputError(null);
        window.setTimeout(() => void refreshOutput(), 300);
        window.setTimeout(() => void refreshFleet(), 600);
      } catch (error) {
        if (selectedIdRef.current === targetSessionId) setOutputError(String(error));
      } finally {
        setSending(false);
      }
    },
    [refreshFleet, refreshOutput, selected?.id, selected?.pane_id, sending],
  );

  // Re-parsing the transcript every second is wasteful; check its mtime instead.
  useEffect(() => {
    const path = selected?.transcript_path;
    const sessionId = selected?.id;
    if (viewMode !== "chat" || !path || !sessionId) return;
    const check = () => {
      void invoke<number>("get_timeline_revision", { transcriptPath: path })
        .then((revision) => {
          if (selectedIdRef.current !== sessionId) return;
          if (revision && revision !== revisionRef.current) {
            revisionRef.current = revision;
            void refreshTimeline();
          }
        })
        .catch(() => {
          // The transcript can disappear while a session exits. Fleet refresh owns
          // the user-facing connection state, so this lightweight poll stays quiet.
        });
    };
    check();
    const interval = window.setInterval(check, 1_000);
    return () => window.clearInterval(interval);
  }, [refreshTimeline, selected?.transcript_path, viewMode]);

  useEffect(() => {
    const scrollToLatest = () => {
      const target = viewMode === "chat" ? conversationRef.current : terminalRef.current;
      if (target && pinnedToBottomRef.current[viewMode]) target.scrollTop = target.scrollHeight;
    };
    pinnedToBottomRef.current[viewMode] = true;
    scrollToLatest();
    let secondFrame = 0;
    const firstFrame = window.requestAnimationFrame(() => {
      scrollToLatest();
      secondFrame = window.requestAnimationFrame(scrollToLatest);
    });
    const settledLayout = window.setTimeout(scrollToLatest, 160);
    return () => {
      window.cancelAnimationFrame(firstFrame);
      window.cancelAnimationFrame(secondFrame);
      window.clearTimeout(settledLayout);
    };
  }, [selectedId, viewMode]);

  useEffect(() => {
    if (!pinnedToBottomRef.current[viewMode]) return;
    const target = viewMode === "chat" ? conversationRef.current : terminalRef.current;
    if (target) target.scrollTop = target.scrollHeight;
  }, [output, timeline, viewMode]);

  const trackBottomPin = useCallback((mode: "chat" | "terminal", element: HTMLElement) => {
    pinnedToBottomRef.current[mode] = element.scrollHeight - element.scrollTop - element.clientHeight < 96;
  }, []);

  useEffect(() => {
    let disposed = false;
    let unlistenEvent: (() => void) | undefined;
    let unlistenConnection: (() => void) | undefined;

    void listen("herdr-event", () => {
      scheduleFleetRefresh();
      if (viewMode === "terminal") void refreshOutput();
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlistenEvent = cleanup;
    });

    void listen<{ connected: boolean }>("herdr-connection", (message) => {
      setSocketConnected(message.payload.connected);
      if (message.payload.connected) scheduleFleetRefresh();
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlistenConnection = cleanup;
    });

    return () => {
      disposed = true;
      unlistenEvent?.();
      unlistenConnection?.();
      if (fleetRefreshTimer.current !== null) {
        window.clearTimeout(fleetRefreshTimer.current);
        fleetRefreshTimer.current = null;
      }
    };
  }, [refreshOutput, scheduleFleetRefresh, viewMode]);

  useEffect(() => {
    if (!mobileSidebarOpen) return;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setMobileSidebarOpen(false);
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [mobileSidebarOpen]);

  // The native "Settings…" menu item and Cmd/Ctrl+, open the panel; Escape closes it.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen("open-settings", () => setSettingsOpen(true)).then((cleanup) => {
      unlisten = cleanup;
    });
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key === ",") {
        event.preventDefault();
        setSettingsOpen(true);
      } else if (event.key === "Escape") {
        setSettingsOpen(false);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      unlisten?.();
      window.removeEventListener("keydown", onKeyDown);
    };
  }, []);

  async function submitMessage(event: FormEvent) {
    event.preventDefault();
    const body = message.trim();
    if (!body || !selected?.pane_id || sending) return;
    const targetSessionId = selected.id;
    const targetPaneId = selected.pane_id;

    setSending(true);
    try {
      await invoke("send_message", { paneId: targetPaneId, text: body });
      setDrafts((current) => {
        const next = { ...current };
        delete next[targetSessionId];
        return next;
      });
      if (selectedIdRef.current === targetSessionId) setOutputError(null);
      window.setTimeout(() => void refreshTimeline(), 400);
      window.setTimeout(() => void refreshFleet(), 500);
    } catch (error) {
      if (selectedIdRef.current === targetSessionId) setOutputError(String(error));
    } finally {
      setSending(false);
    }
  }

  return (
    <div className="app-shell">
      <aside className={`sidebar ${mobileSidebarOpen ? "mobile-open" : ""}`}>
        <div className="brand-row">
          <span className="herdr-status" title={herdrStatus.label}>
            herdr
            <i className={herdrStatus.tone} role="img" aria-label={herdrStatus.label} />
          </span>
          <button className="icon-button" aria-label="새로고침" onClick={() => void refreshFleet()}>↻</button>
          <button
            type="button"
            className="icon-button"
            aria-label="설정"
            aria-expanded={settingsOpen}
            onClick={() => setSettingsOpen((open) => !open)}
          >
            <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
              <circle cx="12" cy="12" r="3" />
              <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
            </svg>
          </button>
        </div>

        {/* Settings render as a modal at the app root (see below). */}

        {/* Styling a WebKit scrollbar at all costs it the OS overlay behavior, so the
            "only while scrolling" fade is driven here instead. */}
        <div className="session-pane">
        {(["above", "below"] as const).map((direction) =>
          offscreenBlocked[direction] > 0 ? (
            <button
              key={direction}
              type="button"
              className={`blocked-peek ${direction}`}
              onClick={() => scrollToBlocked(direction)}
            >
              <span className="peek-arrow" aria-hidden="true">{direction === "above" ? "▲" : "▼"}</span>
              <span className="peek-dot" aria-hidden="true">◉</span>
              {offscreenBlocked[direction]}개 대기 중
            </button>
          ) : null,
        )}
        <nav
          ref={listRef}
          className={`session-list ${listScrolling ? "scrolling" : ""}`}
          aria-label="에이전트 세션"
          onScroll={() => {
            setListScrolling(true);
            syncOffscreenBlocked();
            if (scrollIdleTimer.current !== null) window.clearTimeout(scrollIdleTimer.current);
            scrollIdleTimer.current = window.setTimeout(() => setListScrolling(false), 700);
          }}
        >
          {groups.map((group) => {
            const groupState = rollUpState(group.sessions.map((session) => asState(session.status)));
            const blockedCount = group.sessions.filter(
              (session) => asState(session.status) === "blocked",
            ).length;
            const groupCollapsed = collapsed.includes(group.key);
            return (
            <section
              key={group.key}
              data-workspace-key={group.key}
              className={`workspace-group ${dragTarget?.key === group.key ? (dragTarget.after ? "drop-after" : "drop-before") : ""}`}
            >
              {/* A collapsed group hides its rows, so it stands in for the blocked agents inside it. */}
              <div
                className="group-header-row"
                data-blocked={groupCollapsed && blockedCount > 0 ? blockedCount : undefined}
              >
                <button
                  type="button"
                  className="group-header"
                  aria-expanded={!groupCollapsed}
                  aria-roledescription="드래그 가능한 워크스페이스"
                  title="드래그해서 워크스페이스 순서 변경"
                  onPointerDown={(event) => {
                    if (event.button !== 0) return;
                    pointerDragRef.current = {
                      key: group.key,
                      pointerId: event.pointerId,
                      startY: event.clientY,
                      active: false,
                    };
                    event.currentTarget.setPointerCapture(event.pointerId);
                  }}
                  onPointerMove={(event) => {
                    const drag = pointerDragRef.current;
                    if (!drag || drag.pointerId !== event.pointerId) return;
                    if (!drag.active && Math.abs(event.clientY - drag.startY) > 5) {
                      drag.active = true;
                    }
                    if (!drag.active) return;
                    event.preventDefault();
                    const target = workspaceDropTargetAt(event.clientX, event.clientY);
                    setDragTarget(target && target.key !== drag.key ? target : null);
                  }}
                  onPointerUp={(event) => {
                    const drag = pointerDragRef.current;
                    if (!drag || drag.pointerId !== event.pointerId) return;
                    const wasDragging = drag.active;
                    if (wasDragging) {
                      const target = workspaceDropTargetAt(event.clientX, event.clientY);
                      if (target && target.key !== drag.key) moveWorkspace(target.key, target.after);
                    }
                    pointerDragRef.current = null;
                    setDragTarget(null);
                    suppressWorkspaceClickRef.current = wasDragging;
                    window.setTimeout(() => {
                      suppressWorkspaceClickRef.current = false;
                    }, 0);
                  }}
                  onPointerCancel={() => {
                    pointerDragRef.current = null;
                    setDragTarget(null);
                  }}
                  onClick={(event) => {
                    if (suppressWorkspaceClickRef.current) {
                      event.preventDefault();
                      return;
                    }
                    toggleCollapsed(group.key);
                  }}
                >
                  <span className="drag-grip" aria-hidden="true">⠿</span>
                  <span className={`chevron ${groupCollapsed ? "" : "open"}`} aria-hidden="true">
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.6" strokeLinecap="round" strokeLinejoin="round">
                      <path d="M9 6l6 6-6 6" />
                    </svg>
                  </span>
                  <span className={`group-dot ${stateColorClass[groupState]}`} aria-hidden="true">
                    {stateDot(groupState)}
                  </span>
                  <span className="group-name">{group.label}</span>
                  {groupCollapsed && blockedCount > 0 && (
                    <span className="group-blocked" aria-label={`대기 중 ${blockedCount}개`}>
                      ◉ {blockedCount}
                    </span>
                  )}
                  <span className="group-count">{group.sessions.length}</span>
                </button>
                <button
                  type="button"
                  className={`group-pin ${group.pinned ? "pinned" : ""}`}
                  aria-label={group.pinned ? `${group.label} 고정 해제` : `${group.label} 위로 고정`}
                  title={group.pinned ? "고정 해제" : "위로 고정"}
                  onClick={() => togglePinned(group.key)}
                >
                  {group.pinned ? "★" : "☆"}
                </button>
              </div>
              {!groupCollapsed && (
                <div className="workspace-children">
                  {group.sessions.map((session) => {
                    const ds = asState(session.status);
                    return (
                    <button
                      className={`session-item ${selectedId === session.id ? "selected" : ""}`}
                      data-state={ds}
                      data-blocked={ds === "blocked" ? 1 : undefined}
                      key={session.id}
                      title={session.cwd}
                      onClick={() => {
                        setSelectedId(session.id);
                        setMobileSidebarOpen(false);
                      }}
                    >
                      <span className={`provider-mark ${providerOf(session)}`}>
                        <ProviderLogo provider={providerOf(session)} />
                      </span>
                      <span className="session-copy">
                        <span className="session-title-row">
                          <strong>{session.tab_label ?? session.display_name}</strong>
                          <time>{relativeTime(session.updated_at)}</time>
                        </span>
                        <span className="session-summary">
                          {session.branch ? `⎇ ${session.branch}` : shortPath(session.cwd)}
                        </span>
                        <span className="session-meta">
                          <span className={`agent-glyph ${stateColorClass[ds]}`} aria-hidden="true">
                            {agentIcon(ds, spinnerTick)}
                          </span>
                          <span className={`state-text ${stateColorClass[ds]}`}>{stateLabels[ds]}</span>
                          <span className="meta-dim"> · {session.agent}</span>
                        </span>
                      </span>
                    </button>
                    );
                  })}
                </div>
              )}
            </section>
            );
          })}
          {!sessions && !connectionError && <div className="sidebar-empty">herdr 세션을 불러오는 중…</div>}
          {sessions?.length === 0 && <div className="sidebar-empty">실행 중인 에이전트가 없습니다.</div>}
        </nav>
        </div>

        <div className="provider-strip">
          {(["claude", "codex", "gemini"] as const).map((provider) => {
            const installation = providers.find((item) => item.provider === provider);
            return (
              <div key={provider} title={installation?.version ?? "감지되지 않음"}>
                <i className={installation?.installed ? "online" : "offline"} />
                {providerLabels[provider]}
              </div>
            );
          })}
        </div>
      </aside>
      {mobileSidebarOpen && (
        <button
          type="button"
          className="mobile-sidebar-backdrop"
          aria-label="세션 목록 닫기"
          onClick={() => setMobileSidebarOpen(false)}
        />
      )}

      <main className="workspace">
        {selected ? (
          <>
            <SpaceTabs sessions={sessions ?? []} selected={selected} onSelect={setSelectedId} />

            <header className="topbar">
              <button
                type="button"
                className="mobile-menu-button"
                aria-label="세션 목록 열기"
                aria-expanded={mobileSidebarOpen}
                onClick={() => setMobileSidebarOpen(true)}
              >☰</button>
              <span className={`status-pill ${stateColorClass[asState(selected.status)]}`}>
                <i /> {stateLabels[asState(selected.status)]}
              </span>
              <h1>{selected.tab_label ?? selected.display_name}</h1>
              {/* Model and effort are blank on the CLIs that never write them — say nothing
                  rather than show a "—" the user can't act on. */}
              <p title={selected.cwd}>
                {[
                  providerLabelOf(selected),
                  timeline?.model,
                  timeline?.effort,
                  selected.branch && `⎇ ${selected.branch}`,
                  shortPath(selected.cwd),
                ]
                  .filter(Boolean)
                  .join(" · ")}
              </p>
              <button className="quiet-button" aria-label="새로고침" onClick={() => void refreshOutput()}>↻</button>
            </header>

            <section className={`terminal-section ${viewMode}-mode`} aria-label="에이전트 라이브 출력">
              <div className="terminal-heading">
                <div>
                  <span className={`live-dot ${socketConnected === false ? "disconnected" : ""}`} />
                  <strong>{viewMode === "chat" ? "대화 타임라인" : "라이브 출력"}</strong>
                </div>
                <div className="view-switcher">
                  <button
                    type="button"
                    className={viewMode === "chat" ? "active" : ""}
                    disabled={!selected.transcript_path}
                    title={selected.transcript_path ? "대화 보기" : "트랜스크립트가 없는 세션입니다"}
                    onClick={() => setViewMode("chat")}
                  >대화</button>
                  <button
                    type="button"
                    className={viewMode === "terminal" ? "active" : ""}
                    disabled={!selected.pane_id}
                    title={selected.pane_id ? "터미널 출력 보기" : "herdr 밖 세션입니다"}
                    onClick={() => setViewMode("terminal")}
                  >터미널</button>
                </div>
              </div>

              {viewMode === "chat" ? (
                timeline ? (
                  <div
                    className="conversation-timeline"
                    ref={conversationRef}
                    onScroll={(event) => trackBottomPin("chat", event.currentTarget)}
                  >
                    <div className="timeline-source">Claude transcript · {timeline.items.length}개 이벤트</div>
                    {timeline.items.map((event, index) => (
                      <article className={`conversation-event ${event.role}`} key={`${index}-${event.ts ?? ""}`}>
                        {event.role === "tool" ? (
                          <div className="tool-card">
                            <div>
                              <span>›_</span>
                              <strong>{event.tool_name ?? "Tool"}</strong>
                              <time>{formatEventTime(event.ts)}</time>
                            </div>
                            <pre>{event.text}</pre>
                          </div>
                        ) : (
                          <div className="message-card">
                            <div className="message-meta">
                              <strong>{event.role === "user" ? "나" : providerLabelOf(selected)}</strong>
                              <time>{formatEventTime(event.ts)}</time>
                            </div>
                            <MarkdownMessage content={event.text} theme={theme} collapsible={event.role === "user"} />
                          </div>
                        )}
                      </article>
                    ))}
                    {timeline.items.length === 0 && <div className="output-state">표시할 대화 이벤트가 아직 없습니다.</div>}
                  </div>
                ) : (
                  <div className="output-state">대화를 불러오는 중…</div>
                )
              ) : outputError ? (
                <div className="output-state error-state">{outputError}</div>
              ) : output !== null ? (
                <pre
                  className="terminal-output"
                  ref={terminalRef}
                  onScroll={(event) => trackBottomPin("terminal", event.currentTarget)}
                >{output || "아직 표시할 출력이 없습니다."}</pre>
              ) : (
                <div className="output-state">최근 출력을 불러오는 중…</div>
              )}
            </section>

            {blocked && selected.pane_id && (
              <div className="prompt-bar">
                <div className="prompt-bar-inner">
                  <span className="prompt-bar-title">에이전트가 응답을 기다리는 중</span>
                  {choices.length > 0 && (
                    <div className="prompt-choices">
                      {choices.map((choice) => (
                        <button
                          key={choice.key}
                          type="button"
                          className={choice.active ? "prompt-choice current" : "prompt-choice"}
                          disabled={sending}
                          onClick={() => void sendKeys([choice.key])}
                        >
                          <b>{choice.key}</b>
                          <span>{choice.label}</span>
                        </button>
                      ))}
                    </div>
                  )}
                  {/* The parser can miss a prompt shape we haven't seen; arrows and Enter drive
                      any of the three menus without reading a single line of it. */}
                  <div className="prompt-nav">
                    <button type="button" disabled={sending} onClick={() => void sendKeys(["up"])} aria-label="위로">↑</button>
                    <button type="button" disabled={sending} onClick={() => void sendKeys(["down"])} aria-label="아래로">↓</button>
                    <button type="button" disabled={sending} onClick={() => void sendKeys(["enter"])}>⏎ 선택</button>
                    <button type="button" disabled={sending} onClick={() => void sendKeys(["esc"])}>esc 취소</button>
                  </div>
                </div>
              </div>
            )}

            <form className="composer" onSubmit={submitMessage}>
              <div className="composer-inner">
                <textarea
                  aria-label="에이전트에게 메시지 보내기"
                  placeholder={
                    selected.pane_id
                      ? `${providerLabelOf(selected)}에 지시하기…`
                      : "읽기 전용 — herdr 밖 세션"
                  }
                  value={message}
                  disabled={sending || !selected.pane_id}
                  onChange={(event) => {
                    if (!selectedId) return;
                    const value = event.currentTarget.value;
                    setDrafts((current) => ({ ...current, [selectedId]: value }));
                  }}
                  onKeyDown={(event) => {
                    // Enter also commits a Korean IME composition — don't send mid-word.
                    if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                      event.preventDefault();
                      event.currentTarget.form?.requestSubmit();
                    }
                  }}
                />
                <div className="composer-actions">
                  <span>{sending ? "전송 중…" : "↵ 전송 · ⇧↵ 줄바꿈"}</span>
                  <button
                    type="submit"
                    aria-label="보내기"
                    disabled={sending || !message.trim() || !selected.pane_id}
                  >↑</button>
                </div>
              </div>
            </form>
          </>
        ) : (
          <section className="empty-workspace">
            <div className="brand-mark">H</div>
            <h1>{connectionError ? "herdr에 연결할 수 없습니다" : "실행 중인 세션이 없습니다"}</h1>
            <p>{connectionError ?? "herdr pane에서 Claude Code를 실행하면 여기에 나타납니다."}</p>
            <button onClick={() => void refreshFleet()}>다시 연결</button>
          </section>
        )}
      </main>

      {settingsOpen && (
        <div className="settings-overlay" role="presentation" onClick={() => setSettingsOpen(false)}>
          <div
            className="settings-modal"
            role="dialog"
            aria-modal="true"
            aria-label="설정"
            onClick={(event) => event.stopPropagation()}
          >
            <div className="settings-modal-head">
              <h2>설정</h2>
              <button type="button" className="icon-button" aria-label="닫기" onClick={() => setSettingsOpen(false)}>✕</button>
            </div>
            <div className="settings-row">
              <div className="settings-row-label">
                <span className="settings-row-title">다크 모드</span>
                <span className="settings-row-sub">어두운 테마로 전환 · ⌘,</span>
              </div>
              <button
                type="button"
                role="switch"
                aria-checked={theme === "dark"}
                aria-label="다크 모드"
                className={`theme-toggle ${theme === "dark" ? "on" : ""}`}
                onClick={() => setTheme((current) => (current === "dark" ? "light" : "dark"))}
              />
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default App;
