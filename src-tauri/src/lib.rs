mod event_stream;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

/// Locates the herdr binary the same way herdr's own installers place it.
/// Falls back to bare "herdr" so a PATH install still works.
fn herdr_bin() -> PathBuf {
    if let Ok(path) = std::env::var("HERDR_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".local/bin/herdr");
        if path.is_file() {
            return path;
        }
    }
    ["/opt/homebrew/bin/herdr", "/usr/local/bin/herdr"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("herdr"))
}

/// Runs a herdr subcommand, returning its stdout. Errors carry herdr's stderr.
fn herdr_run(args: &[&str]) -> Result<Vec<u8>, String> {
    let output = std::process::Command::new(herdr_bin())
        .args(args)
        .output()
        .map_err(|e| format!("herdr를 실행할 수 없습니다: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(output.stdout)
}

/// Same, for the subcommands that answer with JSON. The write commands (`pane send-text`,
/// `pane send-keys`) print nothing on success — use herdr_run for those, not this.
fn herdr_json(args: &[&str]) -> Result<Value, String> {
    let stdout = herdr_run(args)?;
    serde_json::from_slice(&stdout).map_err(|e| format!("herdr 응답을 해석할 수 없습니다: {e}"))
}

// ---- API types (contract with the frontend) ----

#[derive(Serialize, Clone)]
struct SessionView {
    id: String,
    session_id: Option<String>,
    name: String,
    agent: String,
    cwd: String,
    status: String,
    source: String,
    pane_id: Option<String>,
    pid: Option<i32>,
    transcript_path: Option<String>,
    updated_at: Option<i64>,
    workspace: Option<String>,
    workspace_id: Option<String>,
    workspace_number: Option<i64>,
    tab_label: Option<String>,
    display_name: String,
    branch: Option<String>,
}

#[derive(Serialize, Clone, PartialEq, Debug)]
struct TimelineItem {
    role: String,
    text: String,
    ts: Option<String>,
    /// Only set for role="tool" — the UI renders it as the tool card's title.
    tool_name: Option<String>,
}

#[derive(Serialize, Clone)]
struct ProviderInstallation {
    provider: String,
    installed: bool,
    version: Option<String>,
}

// ---- data source shapes ----

#[derive(Deserialize)]
struct RegistryEntry {
    pid: i32,
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(rename = "updatedAt", default)]
    updated_at: Option<i64>,
}

#[derive(Deserialize)]
struct HerdrAgent {
    agent: String,
    agent_status: String,
    cwd: String,
    pane_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    tab_id: Option<String>,
}

#[derive(Deserialize)]
struct HerdrWorkspace {
    workspace_id: String,
    label: String,
    number: i64,
}

#[derive(Deserialize)]
struct HerdrTab {
    tab_id: String,
    label: String,
}

// ---- helpers ----

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Checks whether a pid is still alive: kill(pid, 0) == 0 or errno == EPERM means alive.
fn is_process_alive(pid: i32) -> bool {
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn resolve_name(
    registry_name: Option<&str>,
    herdr_name: Option<&str>,
    herdr_agent: Option<&str>,
    cwd: &str,
) -> String {
    for name in [registry_name, herdr_name, herdr_agent]
        .into_iter()
        .flatten()
    {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    basename(cwd)
}

/// Builds the herdr-flavored display name: "{workspace} {tab_label}" when both are known,
/// else the registry name, else the cwd's last path segment.
fn resolve_display_name(
    workspace: Option<&str>,
    tab_label: Option<&str>,
    registry_name: Option<&str>,
    cwd: &str,
) -> String {
    if let (Some(w), Some(t)) = (workspace, tab_label) {
        return format!("{w} {t}");
    }
    if let Some(n) = registry_name {
        if !n.is_empty() {
            return n.to_string();
        }
    }
    basename(cwd)
}

fn merge_status(registry_status: Option<&str>, herdr_status: Option<&str>) -> String {
    if herdr_status == Some("blocked") {
        return "blocked".to_string();
    }
    if registry_status == Some("busy") {
        return "working".to_string();
    }
    if herdr_status == Some("working") {
        return "working".to_string();
    }
    "idle".to_string()
}

fn status_rank(status: &str) -> u8 {
    match status {
        "blocked" => 0,
        "working" => 1,
        _ => 2,
    }
}

fn sort_sessions(sessions: &mut [SessionView]) {
    sessions.sort_by(|a, b| {
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then_with(|| match (a.updated_at, b.updated_at) {
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
    });
}

fn read_registry() -> Vec<RegistryEntry> {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };
    let dir = format!("{home}/.claude/sessions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let parsed: RegistryEntry = match serde_json::from_str(&content) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if is_process_alive(parsed.pid) {
            out.push(parsed);
        }
    }
    out
}

/// One `herdr api snapshot` call gives agents + workspaces + tabs at once.
/// Any failure degrades to empty — the app then runs registry-only.
struct HerdrSnapshot {
    agents: Vec<HerdrAgent>,
    workspaces: HashMap<String, (String, i64)>,
    tabs: HashMap<String, String>,
}

fn read_herdr_snapshot() -> HerdrSnapshot {
    let empty = || HerdrSnapshot {
        agents: Vec::new(),
        workspaces: HashMap::new(),
        tabs: HashMap::new(),
    };
    let v = match herdr_json(&["api", "snapshot"]) {
        Ok(v) => v,
        Err(_) => return empty(),
    };
    let snap = match v.pointer("/result/snapshot") {
        Some(s) => s,
        None => return empty(),
    };
    let take = |key: &str| snap.get(key).cloned().unwrap_or(Value::Null);
    let agents: Vec<HerdrAgent> = serde_json::from_value(take("agents")).unwrap_or_default();
    let workspaces: Vec<HerdrWorkspace> =
        serde_json::from_value(take("workspaces")).unwrap_or_default();
    let tabs: Vec<HerdrTab> = serde_json::from_value(take("tabs")).unwrap_or_default();
    HerdrSnapshot {
        agents,
        workspaces: workspaces
            .into_iter()
            .map(|w| (w.workspace_id, (w.label, w.number)))
            .collect(),
        tabs: tabs.into_iter().map(|t| (t.tab_id, t.label)).collect(),
    }
}

/// Cache of pane_id -> (fpgid, fetched_at). A pane's foreground process group leader
/// only changes when the user restarts the agent in that pane, so 10s staleness is fine.
type FpgidCache = Mutex<HashMap<String, (Option<i32>, std::time::Instant)>>;
static FPGID_CACHE: std::sync::OnceLock<FpgidCache> = std::sync::OnceLock::new();

/// Foreground process group id of a herdr pane via `herdr pane process-info`.
/// This equals the registry session's pid when claude runs in that pane — the exact
/// join key between registry sessions and herdr panes. Cached for 10s per pane.
fn pane_fpgid(pane_id: &str) -> Option<i32> {
    let cache = FPGID_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap();
    if let Some((fpgid, fetched_at)) = map.get(pane_id) {
        if fetched_at.elapsed() < std::time::Duration::from_secs(10) {
            return *fpgid;
        }
    }
    let fpgid = herdr_json(&["pane", "process-info", "--pane", pane_id])
        .ok()
        .and_then(|v| {
            v["result"]["process_info"]["foreground_process_group_id"]
                .as_i64()
                .map(|n| n as i32)
        });
    map.insert(pane_id.to_string(), (fpgid, std::time::Instant::now()));
    fpgid
}

/// Maps pane_id -> fpgid for every herdr agent pane (cached lookups).
fn read_pane_fpgids(herdr: &[HerdrAgent]) -> HashMap<String, i32> {
    let mut out = HashMap::new();
    for h in herdr {
        if let Some(fpgid) = pane_fpgid(&h.pane_id) {
            out.insert(h.pane_id.clone(), fpgid);
        }
    }
    out
}

/// Cache of cwd -> (branch, fetched_at) so `git rev-parse` doesn't run on every poll.
type BranchCache = Mutex<HashMap<String, (Option<String>, std::time::Instant)>>;
static BRANCH_CACHE: std::sync::OnceLock<BranchCache> = std::sync::OnceLock::new();

/// Current branch for a cwd via `git rev-parse --abbrev-ref HEAD`, cached for 30s.
/// None if the cwd isn't a git repo or git isn't available.
fn git_branch(cwd: &str) -> Option<String> {
    let cache = BRANCH_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap();
    if let Some((branch, fetched_at)) = map.get(cwd) {
        if fetched_at.elapsed() < std::time::Duration::from_secs(30) {
            return branch.clone();
        }
    }
    let branch = std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    map.insert(cwd.to_string(), (branch.clone(), std::time::Instant::now()));
    branch
}

/// Finds the transcript path for a session id by scanning ~/.claude/projects/*/<id>.jsonl.
/// Uses the most recently modified match if somehow more than one exists.
fn find_transcript(session_id: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let projects_dir = format!("{home}/.claude/projects");
    let entries = std::fs::read_dir(&projects_dir).ok()?;

    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join(format!("{session_id}.jsonl"));
        let meta = match std::fs::metadata(&candidate) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let candidate_str = candidate.to_string_lossy().to_string();
        match &best {
            Some((best_mtime, _)) if *best_mtime >= mtime => {}
            _ => best = Some((mtime, candidate_str)),
        }
    }
    best.map(|(_, p)| p)
}

/// Looks up a herdr agent's workspace label/number and tab label from the id maps built
/// from `herdr workspace list` / `herdr tab list`.
fn herdr_workspace_fields(
    h: &HerdrAgent,
    workspaces: &HashMap<String, (String, i64)>,
    tabs: &HashMap<String, String>,
) -> (Option<String>, Option<i64>, Option<String>) {
    let workspace_entry = h.workspace_id.as_deref().and_then(|id| workspaces.get(id));
    let workspace_label = workspace_entry.map(|(l, _)| l.clone());
    let workspace_number = workspace_entry.map(|(_, n)| *n);
    let tab_label = h.tab_id.as_deref().and_then(|id| tabs.get(id)).cloned();
    (workspace_label, workspace_number, tab_label)
}

/// Merges registry sessions and herdr agents into the SessionView contract.
/// A registry session and a herdr agent are matched into source="both" when the
/// registry pid equals the pane's foreground process group id — an exact 1:1 join
/// (the claude process IS the pane's foreground process group leader).
fn build_sessions(
    registry: Vec<RegistryEntry>,
    herdr: Vec<HerdrAgent>,
    workspaces: &HashMap<String, (String, i64)>,
    tabs: &HashMap<String, String>,
    fpgids: &HashMap<String, i32>,
) -> Vec<SessionView> {
    let mut herdr_by_pid: HashMap<i32, &HerdrAgent> = HashMap::new();
    for h in &herdr {
        if let Some(fpgid) = fpgids.get(&h.pane_id) {
            herdr_by_pid.insert(*fpgid, h);
        }
    }

    let mut matched_panes: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();

    for r in &registry {
        if let Some(h) = herdr_by_pid.get(&r.pid).copied() {
            matched_panes.insert(h.pane_id.as_str());
            let (workspace, workspace_number, tab_label) =
                herdr_workspace_fields(h, workspaces, tabs);
            out.push(SessionView {
                id: r.session_id.clone(),
                session_id: Some(r.session_id.clone()),
                name: resolve_name(r.name.as_deref(), h.name.as_deref(), Some(&h.agent), &r.cwd),
                agent: "claude".to_string(),
                cwd: r.cwd.clone(),
                status: merge_status(r.status.as_deref(), Some(&h.agent_status)),
                source: "both".to_string(),
                pane_id: Some(h.pane_id.clone()),
                pid: Some(r.pid),
                transcript_path: find_transcript(&r.session_id),
                updated_at: r.updated_at,
                display_name: resolve_display_name(
                    workspace.as_deref(),
                    tab_label.as_deref(),
                    r.name.as_deref(),
                    &r.cwd,
                ),
                workspace,
                workspace_id: h.workspace_id.clone(),
                workspace_number,
                tab_label,
                branch: git_branch(&r.cwd),
            });
        } else {
            out.push(SessionView {
                id: r.session_id.clone(),
                session_id: Some(r.session_id.clone()),
                name: resolve_name(r.name.as_deref(), None, None, &r.cwd),
                agent: "claude".to_string(),
                cwd: r.cwd.clone(),
                status: merge_status(r.status.as_deref(), None),
                source: "registry".to_string(),
                pane_id: None,
                pid: Some(r.pid),
                transcript_path: find_transcript(&r.session_id),
                updated_at: r.updated_at,
                display_name: resolve_display_name(None, None, r.name.as_deref(), &r.cwd),
                workspace: None,
                workspace_id: None,
                workspace_number: None,
                tab_label: None,
                branch: git_branch(&r.cwd),
            });
        }
    }

    for h in &herdr {
        if matched_panes.contains(h.pane_id.as_str()) {
            continue;
        }
        let (workspace, workspace_number, tab_label) = herdr_workspace_fields(h, workspaces, tabs);
        out.push(SessionView {
            id: h.pane_id.clone(),
            session_id: None,
            name: resolve_name(None, h.name.as_deref(), Some(&h.agent), &h.cwd),
            agent: h.agent.clone(),
            cwd: h.cwd.clone(),
            status: merge_status(None, Some(&h.agent_status)),
            source: "herdr".to_string(),
            pane_id: Some(h.pane_id.clone()),
            pid: None,
            transcript_path: None,
            updated_at: None,
            display_name: resolve_display_name(
                workspace.as_deref(),
                tab_label.as_deref(),
                None,
                &h.cwd,
            ),
            workspace,
            workspace_id: h.workspace_id.clone(),
            workspace_number,
            tab_label,
            branch: git_branch(&h.cwd),
        });
    }

    sort_sessions(&mut out);
    out
}

#[tauri::command]
fn list_sessions() -> Vec<SessionView> {
    let snap = read_herdr_snapshot();
    let fpgids = read_pane_fpgids(&snap.agents);
    build_sessions(
        read_registry(),
        snap.agents,
        &snap.workspaces,
        &snap.tabs,
        &fpgids,
    )
}

// ---- timeline parsing ----

/// Only the tail of a transcript is worth reading — sessions can grow past 5MB and the
/// UI shows the last N turns anyway.
const MAX_TRANSCRIPT_BYTES: u64 = 768 * 1024;

fn validate_transcript_path_with_root(
    path: &std::path::Path,
    root: &std::path::Path,
) -> Result<PathBuf, String> {
    if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
        return Err("Claude transcript JSONL 파일만 읽을 수 있습니다".to_string());
    }
    let root = std::fs::canonicalize(root)
        .map_err(|_| "Claude projects 경로를 찾을 수 없습니다".to_string())?;
    let path = std::fs::canonicalize(path)
        .map_err(|_| "트랜스크립트 파일을 찾을 수 없습니다".to_string())?;
    if !path.starts_with(&root) {
        return Err("허용되지 않은 트랜스크립트 경로입니다".to_string());
    }
    Ok(path)
}

fn validated_transcript_path(path: &str) -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME 경로를 찾을 수 없습니다".to_string())?;
    let root = PathBuf::from(home).join(".claude/projects");
    validate_transcript_path_with_root(std::path::Path::new(path), &root)
}

fn truncate(value: &str, limit: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= limit {
        return value.to_string();
    }
    format!("{}…", value.chars().take(limit).collect::<String>())
}

/// Prefers the one input field that identifies what the tool actually did.
fn tool_summary(block: &Value) -> String {
    let input = block.get("input").unwrap_or(&Value::Null);
    for key in ["command", "file_path", "path", "query", "pattern", "url"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return truncate(value, 360);
        }
    }
    truncate(&serde_json::to_string(input).unwrap_or_default(), 360)
}

/// Harness plumbing that Claude Code injects into the transcript as user turns.
fn ignored_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<system-reminder")
        || trimmed.starts_with("<local-command")
        || trimmed.starts_with("<ide_")
}

fn between<'a>(value: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let offset = value.find(start)? + start.len();
    let remainder = &value[offset..];
    let length = remainder.find(end)?;
    Some(&remainder[..length])
}

/// Slash commands arrive as XML; render them back as the user typed them.
fn normalized_user_text(text: &str) -> String {
    if let Some(command) = between(text, "<command-name>", "</command-name>") {
        let args = between(text, "<command-args>", "</command-args>")
            .unwrap_or_default()
            .trim();
        return if args.is_empty() {
            command.trim().to_string()
        } else {
            format!("{} {args}", command.trim())
        };
    }
    truncate(text, 8_000)
}

/// Parses a single JSONL transcript line into zero or more timeline items.
/// Never panics: unparsable or irrelevant lines simply yield an empty Vec.
fn parse_line(line: &str) -> Vec<TimelineItem> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let role = match v.get("type").and_then(|t| t.as_str()) {
        Some(t @ ("user" | "assistant")) => t.to_string(),
        _ => return Vec::new(),
    };
    if v.get("isSidechain")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
    {
        return Vec::new();
    }
    if v.get("isMeta").and_then(|b| b.as_bool()).unwrap_or(false) {
        return Vec::new();
    }

    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string());

    let content = match v.get("message").and_then(|m| m.get("content")) {
        Some(c) => c,
        None => return Vec::new(),
    };

    // A plain-string content is the same as a single text block.
    let blocks: Vec<Value> = if let Some(text) = content.as_str() {
        vec![serde_json::json!({ "type": "text", "text": text })]
    } else {
        content.as_array().cloned().unwrap_or_default()
    };

    let mut items = Vec::new();
    for block in blocks {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match (role.as_str(), block_type) {
            ("user", "text") => {
                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !text.is_empty() && !ignored_user_text(text) {
                    items.push(TimelineItem {
                        role: "user".to_string(),
                        text: normalized_user_text(text),
                        ts: ts.clone(),
                        tool_name: None,
                    });
                }
            }
            ("assistant", "text") => {
                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !text.is_empty() {
                    items.push(TimelineItem {
                        role: "assistant".to_string(),
                        text: truncate(text, 12_000),
                        ts: ts.clone(),
                        tool_name: None,
                    });
                }
            }
            ("assistant", "tool_use") => {
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("Tool");
                items.push(TimelineItem {
                    role: "tool".to_string(),
                    text: tool_summary(&block),
                    ts: ts.clone(),
                    tool_name: Some(name.to_string()),
                });
            }
            _ => {} // thinking, tool_result and anything else is skipped
        }
    }
    items
}

/// Reads at most the last MAX_TRANSCRIPT_BYTES, dropping the first (likely truncated) line.
fn read_tail(path: &str) -> Result<String, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    let offset = length.saturating_sub(MAX_TRANSCRIPT_BYTES);
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if offset > 0 {
        if let Some(first_newline) = text.find('\n') {
            text.drain(..=first_newline);
        }
    }
    Ok(text)
}

#[tauri::command]
fn get_timeline(transcript_path: String, last: usize) -> Result<Vec<TimelineItem>, String> {
    let transcript_path = validated_transcript_path(&transcript_path)?;
    let content = read_tail(&transcript_path.to_string_lossy())?;
    let mut items: Vec<TimelineItem> = content.lines().flat_map(parse_line).collect();
    let start = items.len().saturating_sub(last);
    items = items.split_off(start);
    Ok(items)
}

/// Transcript mtime in epoch millis (0 if missing) — a cheap change check so the UI can
/// poll this instead of re-parsing the whole transcript every second.
#[tauri::command]
fn get_timeline_revision(transcript_path: String) -> Result<i64, String> {
    let transcript_path = validated_transcript_path(&transcript_path)?;
    Ok(std::fs::metadata(&transcript_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0))
}

/// Which agent CLIs are installed — drives the sidebar's footer strip.
#[tauri::command]
fn detect_providers() -> Vec<ProviderInstallation> {
    ["claude", "codex", "gemini"]
        .into_iter()
        .map(|provider| {
            let version = std::process::Command::new(provider)
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty());
            ProviderInstallation {
                provider: provider.to_string(),
                installed: version.is_some(),
                version,
            }
        })
        .collect()
}

/// Raw rendered terminal output of a herdr pane.
#[tauri::command]
fn read_pane(pane_id: String) -> Result<String, String> {
    validate_pane_id(&pane_id)?;
    let v = herdr_json(&[
        "agent",
        "read",
        &pane_id,
        "--source",
        "recent-unwrapped",
        "--lines",
        "180",
        "--format",
        "text",
    ])?;
    v.pointer("/result/read/text")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "herdr 응답에 텍스트가 없습니다".to_string())
}

// ---- sending messages ----

/// Validates a herdr pane id shape by hand (no regex crate): ^w[A-Za-z0-9]+:p[0-9]+$.
/// This is a trust-boundary check so a compromised frontend can't pass arbitrary args
/// through to the herdr subprocess.
fn validate_pane_id(pane_id: &str) -> Result<(), String> {
    let rest = match pane_id.strip_prefix('w') {
        Some(r) => r,
        None => return Err("잘못된 pane id".to_string()),
    };
    let mut parts = rest.split(':');
    let workspace_part = parts.next().unwrap_or("");
    let pane_part = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return Err("잘못된 pane id".to_string());
    }
    if workspace_part.is_empty() || !workspace_part.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("잘못된 pane id".to_string());
    }
    let digits = match pane_part.strip_prefix('p') {
        Some(d) => d,
        None => return Err("잘못된 pane id".to_string()),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err("잘못된 pane id".to_string());
    }
    Ok(())
}

const MAX_MESSAGE_CHARS: usize = 32_000;

#[tauri::command]
fn send_message(pane_id: String, text: String) -> Result<(), String> {
    validate_pane_id(&pane_id)?;
    let text = text.trim();
    if text.is_empty() {
        return Err("텍스트가 비어 있습니다".to_string());
    }
    if text.chars().count() > MAX_MESSAGE_CHARS {
        return Err(format!("메시지가 너무 깁니다 (최대 {MAX_MESSAGE_CHARS}자)"));
    }
    // The pane may have closed since the last poll; typing into a recycled pane id would
    // land the message in someone else's session.
    if !read_herdr_snapshot()
        .agents
        .iter()
        .any(|a| a.pane_id == pane_id)
    {
        return Err("이 세션의 pane이 더 이상 존재하지 않습니다".to_string());
    }

    event_stream::send_text_and_enter(&pane_id, text)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            list_sessions,
            get_timeline,
            get_timeline_revision,
            read_pane,
            send_message,
            detect_providers,
            event_stream::herdr_event_status
        ])
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::TrayIconBuilder;
            use tauri::Manager;

            event_stream::start(app.handle().clone());

            let open_item = MenuItem::with_id(app, "open", "열기", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_item, &quit_item])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            if let Some(window) = app.get_webview_window("main") {
                let window_clone = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        let _ = window_clone.hide();
                        api.prevent_close();
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_string_content() {
        let line = r#"{"type":"user","message":{"role":"user","content":"hello there"},"timestamp":"2026-07-13T01:19:41.535Z"}"#;
        let items = parse_line(line);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].role, "user");
        assert_eq!(items[0].text, "hello there");
        assert_eq!(items[0].ts.as_deref(), Some("2026-07-13T01:19:41.535Z"));
    }

    #[test]
    fn parses_assistant_text_and_tool_use() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll check that"},{"type":"tool_use","name":"Bash","input":{"command":"ls -la"}}]},"timestamp":"2026-07-13T01:20:00.000Z"}"#;
        let items = parse_line(line);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].role, "assistant");
        assert_eq!(items[0].text, "I'll check that");
        assert_eq!(items[1].role, "tool");
        assert_eq!(items[1].tool_name.as_deref(), Some("Bash"));
        assert_eq!(items[1].text, "ls -la");
    }

    #[test]
    fn skips_tool_result_blocks_in_user_content() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"ok","is_error":false}]},"timestamp":"2026-07-13T01:21:00.000Z"}"#;
        let items = parse_line(line);
        assert!(items.is_empty());
    }

    #[test]
    fn skips_meta_lines() {
        for line in [
            r#"{"type":"last-prompt","lastPrompt":"진행","sessionId":"abc"}"#,
            r#"{"type":"mode","mode":"normal","sessionId":"abc"}"#,
            r#"{"type":"permission-mode","permissionMode":"auto","sessionId":"abc"}"#,
        ] {
            assert!(parse_line(line).is_empty());
        }
    }

    #[test]
    fn skips_sidechain_and_meta_flagged_lines() {
        let line =
            r#"{"type":"user","message":{"role":"user","content":"hidden"},"isSidechain":true}"#;
        assert!(parse_line(line).is_empty());
        let line = r#"{"type":"user","message":{"role":"user","content":"hidden"},"isMeta":true}"#;
        assert!(parse_line(line).is_empty());
    }

    #[test]
    fn skips_broken_json_lines() {
        assert!(parse_line("{not valid json").is_empty());
        assert!(parse_line("").is_empty());
    }

    #[test]
    fn get_timeline_respects_last_n() {
        let lines = (0..5)
            .map(|i| {
                format!(
                    r#"{{"type":"user","message":{{"role":"user","content":"msg{i}"}},"timestamp":"ts{i}"}}"#
                )
            })
            .collect::<Vec<_>>();
        let items: Vec<TimelineItem> = lines.iter().flat_map(|l| parse_line(l)).collect();
        let start = items.len().saturating_sub(2);
        let last_two = items[start..].to_vec();
        assert_eq!(last_two.len(), 2);
        assert_eq!(last_two[0].text, "msg3");
        assert_eq!(last_two[1].text, "msg4");
    }

    #[test]
    fn validates_pane_id_format() {
        assert!(validate_pane_id("wA:p3").is_ok());
        assert!(validate_pane_id("w9:p12").is_ok());
        assert!(validate_pane_id("wAbc123:p0").is_ok());
    }

    #[test]
    fn rejects_invalid_pane_ids() {
        for bad in [
            "p3",
            "wA:p",
            "wA:3",
            "wA:pX",
            "wA:p3:extra",
            "wA",
            ":p3",
            "wA;p3",
            "wA:p3 ",
            "",
        ] {
            assert!(validate_pane_id(bad).is_err(), "expected error for {bad:?}");
        }
    }

    #[test]
    fn transcript_path_must_be_jsonl_inside_allowed_root() {
        let unique = format!(
            "remote-legion-path-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        let root = base.join("projects");
        let outside = base.join("outside.jsonl");
        let inside = root.join("session.jsonl");
        let wrong_extension = root.join("session.txt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&inside, "{}\n").unwrap();
        std::fs::write(&outside, "{}\n").unwrap();
        std::fs::write(&wrong_extension, "{}\n").unwrap();

        assert_eq!(
            validate_transcript_path_with_root(&inside, &root).unwrap(),
            std::fs::canonicalize(&inside).unwrap()
        );
        assert!(validate_transcript_path_with_root(&outside, &root).is_err());
        assert!(validate_transcript_path_with_root(&wrong_extension, &root).is_err());

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn display_name_prefers_workspace_and_tab() {
        assert_eq!(
            resolve_display_name(
                Some("Home"),
                Some("ssh"),
                Some("registry-name"),
                "/some/cwd"
            ),
            "Home ssh"
        );
    }

    #[test]
    fn display_name_falls_back_to_registry_name_then_cwd_basename() {
        assert_eq!(
            resolve_display_name(None, None, Some("my-session"), "/some/cwd"),
            "my-session"
        );
        assert_eq!(
            resolve_display_name(Some("Home"), None, Some("my-session"), "/some/cwd"),
            "my-session"
        );
        assert_eq!(
            resolve_display_name(None, None, None, "/Users/x/projects/remote-legion"),
            "remote-legion"
        );
    }

    #[test]
    fn skips_harness_injected_user_text() {
        for prefix in [
            "<system-reminder>x</system-reminder>",
            "<local-command-stdout>x",
            "<ide_selection>x",
        ] {
            let line =
                format!(r#"{{"type":"user","message":{{"role":"user","content":"{prefix}"}}}}"#);
            assert!(parse_line(&line).is_empty(), "expected skip for {prefix}");
        }
    }

    #[test]
    fn rebuilds_slash_commands_from_xml() {
        let line = r#"{"type":"user","message":{"role":"user","content":"<command-name>/model</command-name><command-args>opus</command-args>"}}"#;
        let items = parse_line(line);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "/model opus");
    }

    #[test]
    fn tool_summary_prefers_identifying_input_key() {
        let block = serde_json::json!({"input": {"command": "ls -la", "description": "무시됨"}});
        assert_eq!(tool_summary(&block), "ls -la");
        let block = serde_json::json!({"input": {"file_path": "/tmp/x.rs"}});
        assert_eq!(tool_summary(&block), "/tmp/x.rs");
        // 알려진 키가 없으면 input 전체를 직렬화
        let block = serde_json::json!({"input": {"weird": 1}});
        assert_eq!(tool_summary(&block), r#"{"weird":1}"#);
    }

    #[test]
    fn read_tail_drops_partial_first_line() {
        use std::io::Write;
        let path = std::env::temp_dir().join("rl_tail_test.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // 768KB를 넘겨 tail이 잘리도록 채운 뒤, 마지막에 온전한 라인 하나
        let filler = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{}"}}}}"#,
            "x".repeat(1000)
        );
        for _ in 0..900 {
            writeln!(f, "{filler}").unwrap();
        }
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"마지막"}}}}"#
        )
        .unwrap();
        drop(f);

        let text = read_tail(path.to_str().unwrap()).unwrap();
        assert!(text.len() as u64 <= MAX_TRANSCRIPT_BYTES);
        // 모든 라인이 온전한 JSON이어야 함 (잘린 첫 줄이 버려졌다는 뜻)
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            assert!(
                serde_json::from_str::<Value>(line).is_ok(),
                "잘린 라인이 남아있음"
            );
        }
        let items: Vec<TimelineItem> = text.lines().flat_map(parse_line).collect();
        assert_eq!(items.last().unwrap().text, "마지막");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn merges_by_pid_join_even_when_cwds_collide() {
        // 같은 cwd를 공유하는 세션 2개 + pane 2개: cwd 매칭으론 불가능, pid 조인으론 1:1
        let registry: Vec<RegistryEntry> = serde_json::from_value(serde_json::json!([
            {"pid": 100, "sessionId": "s-100", "cwd": "/Users/x", "name": "a", "status": "idle"},
            {"pid": 200, "sessionId": "s-200", "cwd": "/Users/x", "name": "b", "status": "busy"},
            {"pid": 300, "sessionId": "s-300", "cwd": "/Users/x/other", "status": "idle"}
        ]))
        .unwrap();
        let herdr: Vec<HerdrAgent> = serde_json::from_value(serde_json::json!([
            {"agent": "claude", "agent_status": "idle", "cwd": "/Users/x", "pane_id": "w1:p1"},
            {"agent": "claude", "agent_status": "working", "cwd": "/Users/x", "pane_id": "w1:p2"},
            {"agent": "claude", "agent_status": "idle", "cwd": "/elsewhere", "pane_id": "w2:p1"}
        ]))
        .unwrap();
        let fpgids: HashMap<String, i32> =
            [("w1:p1".to_string(), 100), ("w1:p2".to_string(), 200)].into();

        let out = build_sessions(registry, herdr, &HashMap::new(), &HashMap::new(), &fpgids);

        // 4개: 병합 2 + registry 단독 1 + herdr 단독 1 (중복 없음)
        assert_eq!(out.len(), 4);
        let find = |id: &str| out.iter().find(|s| s.id == id).unwrap();
        assert_eq!(find("s-100").source, "both");
        assert_eq!(find("s-100").pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(find("s-200").source, "both");
        assert_eq!(find("s-200").pane_id.as_deref(), Some("w1:p2"));
        assert_eq!(find("s-300").source, "registry");
        assert_eq!(find("w2:p1").source, "herdr");
    }
}
