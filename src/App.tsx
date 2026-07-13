import { FormEvent, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { marked } from "marked";
import DOMPurify from "dompurify";
import "./App.css";

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
  workspace_number: number | null;
  tab_label: string | null;
  display_name: string;
  branch: string | null;
}

interface TimelineItem {
  role: string;
  text: string;
  ts: string | null;
  tool_name: string | null;
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

// herdr's states, rendered with the deck's visual vocabulary.
const statusClass: Record<string, string> = {
  working: "running",
  blocked: "waiting",
  idle: "idle",
  unknown: "unknown",
};

const statusLabels: Record<string, string> = {
  working: "실행 중",
  blocked: "입력 대기",
  idle: "유휴",
  unknown: "알 수 없음",
};

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
        order: session.workspace_number ?? Number.MAX_SAFE_INTEGER,
        sessions: [],
      });
    }
    groups.get(key)!.sessions.push(session);
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

function MarkdownMessage({ content, collapsible = false }: { content: string; collapsible?: boolean }) {
  const contentRef = useRef<HTMLDivElement | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [canCollapse, setCanCollapse] = useState(false);

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

  // Transcripts can quote arbitrary web content, and this webview can send messages
  // into live sessions — never render markdown without sanitizing it first.
  const html = useMemo(
    () => DOMPurify.sanitize(marked.parse(content, { async: false }) as string),
    [content],
  );

  return (
    <>
      <div className={`message-content ${canCollapse && !expanded ? "collapsed" : ""}`} ref={contentRef}>
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
  const [timeline, setTimeline] = useState<TimelineItem[] | null>(null);
  const [viewMode, setViewMode] = useState<"chat" | "terminal">("chat");
  const [mobileSidebarOpen, setMobileSidebarOpen] = useState(false);
  const [pinned, togglePinned, updatePinned] = useStoredKeys("rl.pinnedWorkspaces");
  const [collapsed, toggleCollapsed] = useStoredKeys("rl.collapsedWorkspaces");
  const [workspaceOrder, , updateWorkspaceOrder] = useStoredKeys("rl.workspaceOrder");
  const [dragTarget, setDragTarget] = useState<{ key: string; after: boolean } | null>(null);
  const fleetRefreshTimer = useRef<number | null>(null);
  const fleetRequestRef = useRef(0);
  const revisionRef = useRef<number>(0);
  const conversationRef = useRef<HTMLDivElement | null>(null);
  const terminalRef = useRef<HTMLPreElement | null>(null);
  const selectedIdRef = useRef<string | null>(null);
  const pointerDragRef = useRef<{ key: string; pointerId: number; startY: number; active: boolean } | null>(null);
  const suppressWorkspaceClickRef = useRef(false);
  selectedIdRef.current = selectedId;

  const message = selectedId ? drafts[selectedId] ?? "" : "";

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
      const next = await invoke<TimelineItem[]>("get_timeline", { transcriptPath: path, last: 200 });
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

  useEffect(() => {
    if (viewMode !== "terminal") return;
    void refreshOutput();
    const interval = window.setInterval(() => void refreshOutput(), 2_000);
    return () => window.clearInterval(interval);
  }, [refreshOutput, viewMode]);

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
      if (target) target.scrollTop = target.scrollHeight;
    };
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
  }, [output, selectedId, timeline, viewMode]);

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
          <div className="brand-mark">R</div>
          <div>
            <strong>remote legion</strong>
            <span className={connectionError ? "connection-bad" : "connection-good"}>
              {connectionError
                ? "herdr 연결 끊김"
                : socketConnected
                  ? "herdr · 실시간"
                  : "herdr · 폴링 모드"}
            </span>
          </div>
          <button className="icon-button" aria-label="새로고침" onClick={() => void refreshFleet()}>↻</button>
        </div>

        <nav className="session-list" aria-label="에이전트 세션">
          {groups.map((group) => (
            <section
              key={group.key}
              data-workspace-key={group.key}
              className={`workspace-group ${dragTarget?.key === group.key ? (dragTarget.after ? "drop-after" : "drop-before") : ""}`}
            >
              <div className="group-header-row">
                <button
                  type="button"
                  className="group-header"
                  aria-expanded={!collapsed.includes(group.key)}
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
                  <span className="workspace-icon" aria-hidden="true" />
                  <span className="chevron" aria-hidden="true">
                    {collapsed.includes(group.key) ? "▸" : "▾"}
                  </span>
                  <span className="group-name">{group.label}</span>
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
              {!collapsed.includes(group.key) && (
                <div className="workspace-children">
                  {group.sessions.map((session) => (
                    <button
                      className={`session-item ${selectedId === session.id ? "selected" : ""}`}
                      key={session.id}
                      title={session.cwd}
                      onClick={() => {
                        setSelectedId(session.id);
                        setMobileSidebarOpen(false);
                      }}
                    >
                      <span className={`provider-mark ${providerOf(session)}`}>
                        {providerMarks[providerOf(session)]}
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
                          <i className={`status-dot ${statusClass[session.status] ?? "unknown"}`} />
                          {statusLabels[session.status] ?? session.status} · {shortPath(session.cwd)}
                        </span>
                      </span>
                    </button>
                  ))}
                </div>
              )}
            </section>
          ))}
          {!sessions && !connectionError && <div className="sidebar-empty">herdr 세션을 불러오는 중…</div>}
          {sessions?.length === 0 && <div className="sidebar-empty">실행 중인 에이전트가 없습니다.</div>}
        </nav>

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
            <header className="topbar">
              <button
                type="button"
                className="mobile-menu-button"
                aria-label="세션 목록 열기"
                aria-expanded={mobileSidebarOpen}
                onClick={() => setMobileSidebarOpen(true)}
              >☰</button>
              <div className="mobile-brand"><div className="brand-mark">R</div></div>
              <div className={`provider-mark large ${providerOf(selected)}`}>
                {providerMarks[providerOf(selected)]}
              </div>
              <div className="topbar-title">
                <div>
                  <h1>{selected.display_name}</h1>
                  <span className={`status-pill ${statusClass[selected.status] ?? "unknown"}`}>
                    <i /> {statusLabels[selected.status] ?? selected.status}
                  </span>
                </div>
                <p>
                  {providerLabelOf(selected)} · {selected.workspace ?? "기타"}
                  {selected.tab_label ? ` · ${selected.tab_label}` : ""}
                  {selected.pane_id ? ` · ${selected.pane_id}` : " · herdr 밖 세션"}
                </p>
              </div>
              <button className="quiet-button" aria-label="새로고침" onClick={() => void refreshOutput()}>↻</button>
            </header>

            <section className="briefing">
              <div className="eyebrow">herdr 라이브 상태</div>
              <div className="briefing-grid">
                <div>
                  <span>현재</span>
                  <strong>{statusLabels[selected.status] ?? selected.status}</strong>
                </div>
                <div>
                  <span>작업공간</span>
                  <strong>
                    {selected.workspace ?? "기타"}
                    {selected.tab_label ? ` / ${selected.tab_label}` : ""}
                    {selected.branch ? ` · ⎇ ${selected.branch}` : ""}
                  </strong>
                </div>
                <div>
                  <span>경로</span>
                  <strong title={selected.cwd}>{shortPath(selected.cwd)}</strong>
                </div>
              </div>
            </section>

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
                  <div className="conversation-timeline" ref={conversationRef}>
                    <div className="timeline-source">Claude transcript · {timeline.length}개 이벤트</div>
                    {timeline.map((event, index) => (
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
                            <MarkdownMessage content={event.text} collapsible={event.role === "user"} />
                          </div>
                        )}
                      </article>
                    ))}
                    {timeline.length === 0 && <div className="output-state">표시할 대화 이벤트가 아직 없습니다.</div>}
                  </div>
                ) : (
                  <div className="output-state">대화를 불러오는 중…</div>
                )
              ) : outputError ? (
                <div className="output-state error-state">{outputError}</div>
              ) : output !== null ? (
                <pre className="terminal-output" ref={terminalRef}>{output || "아직 표시할 출력이 없습니다."}</pre>
              ) : (
                <div className="output-state">최근 출력을 불러오는 중…</div>
              )}
            </section>

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
            <div className="brand-mark">R</div>
            <h1>{connectionError ? "herdr에 연결할 수 없습니다" : "실행 중인 세션이 없습니다"}</h1>
            <p>{connectionError ?? "herdr pane에서 Claude Code를 실행하면 여기에 나타납니다."}</p>
            <button onClick={() => void refreshFleet()}>다시 연결</button>
          </section>
        )}
      </main>
    </div>
  );
}

export default App;
