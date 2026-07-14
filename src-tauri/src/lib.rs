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
    workspace_order: Option<i64>,
    tab_id: Option<String>,
    tab_label: Option<String>,
    tab_order: Option<i64>,
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

/// The transcript's events plus what the agent is running on — the topbar shows model/effort, and
/// both are only knowable by reading the transcript (herdr's agent JSON carries neither).
#[derive(Serialize, Clone)]
struct Timeline {
    items: Vec<TimelineItem>,
    model: Option<String>,
    effort: Option<String>,
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
    // herdr's socket authors the full blocked/working/done/idle/unknown vocabulary, so keep it
    // verbatim — but let the registry's own busy flag promote to "working" (it is Claude Code's
    // self-report, more reliable than screen detection when the two disagree).
    if herdr_status == Some("blocked") {
        return "blocked".to_string();
    }
    if herdr_status == Some("working") || registry_status == Some("busy") {
        return "working".to_string();
    }
    match herdr_status {
        Some("done") => "done".to_string(),
        Some("idle") => "idle".to_string(),
        Some("unknown") => "unknown".to_string(),
        _ => "idle".to_string(),
    }
}

fn status_rank(status: &str) -> u8 {
    match status {
        "blocked" => 0,
        "working" => 1,
        "done" => 2,
        "idle" => 3,
        _ => 4,
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
    tabs: HashMap<String, (String, i64)>,
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
    // Both arrays come in herdr's own display order; the `number` field is a creation id that
    // survives reordering, so position — not number — is what herdr shows.
    HerdrSnapshot {
        agents,
        workspaces: workspaces
            .into_iter()
            .enumerate()
            .map(|(i, w)| (w.workspace_id, (w.label, i as i64)))
            .collect(),
        tabs: tabs
            .into_iter()
            .enumerate()
            .map(|(i, t)| (t.tab_id, (t.label, i as i64)))
            .collect(),
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

/// Scans ~/.codex/sessions/**/rollout-*.jsonl and returns the most recently modified file whose
/// recorded session cwd matches. Codex panes carry no session id, so cwd + recency is the join key.
fn find_codex_transcript(cwd: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let root = PathBuf::from(&home).join(".codex/sessions");
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    collect_rollout_files(&root, 0, &mut files);
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.into_iter().take(60) {
        if codex_session_cwd(&path).as_deref() == Some(cwd) {
            return Some(path.to_string_lossy().to_string());
        }
    }
    None
}

fn collect_rollout_files(
    dir: &std::path::Path,
    depth: u8,
    out: &mut Vec<(std::time::SystemTime, PathBuf)>,
) {
    if depth > 6 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, depth + 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("rollout-"))
                .unwrap_or(false)
        {
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            out.push((mtime, path));
        }
    }
}

/// Reads the recorded working directory from a codex rollout's session-meta line (near the top).
fn codex_session_cwd(path: &std::path::Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(8).flatten() {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for cwd in [
            v.pointer("/payload/cwd"),
            v.pointer("/payload/session/cwd"),
            v.get("cwd"),
            v.pointer("/meta/cwd"),
        ] {
            if let Some(s) = cwd.and_then(Value::as_str) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Gemini CLI stores per-project chats under ~/.gemini/tmp/<project-name>/, where the folder is the
/// project directory's basename, slugified (lowercased, non-alphanumerics → '-'). We resolve that
/// folder and pick its conversation JSON (chats / checkpoint / logs.json), most complete then most
/// recent winning.
fn find_gemini_transcript(cwd: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let base = cwd.rsplit('/').find(|s| !s.is_empty())?.to_string();
    let slug: String = base
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let tmp = PathBuf::from(&home).join(".gemini/tmp");
    let mut best: Option<(u8, std::time::SystemTime, u64, String)> = None;
    for name in [base, slug] {
        gemini_scan_dir(&tmp.join(&name), &mut best);
        if best.is_some() {
            break;
        }
    }
    best.map(|(_, _, _, p)| p)
}

/// Keeps the best conversation file inside a gemini project temp dir: newest wins, and on a tie the
/// larger file wins. Resuming a session leaves behind same-mtime stub chat files holding nothing but
/// the session_context preamble — without the size tiebreak a stub can beat the live conversation.
fn gemini_scan_dir(
    dir: &std::path::Path,
    best: &mut Option<(u8, std::time::SystemTime, u64, String)>,
) {
    let mut stack = vec![dir.to_path_buf()];
    let mut guard = 0;
    while let Some(current) = stack.pop() {
        guard += 1;
        if guard > 128 {
            break;
        }
        let entries = match std::fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            // Recent Gemini CLI writes chats as .jsonl; older builds used .json.
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if extension != "json" && extension != "jsonl" {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let rank: u8 = if name.contains("checkpoint") || path.to_string_lossy().contains("/chats/")
            {
                3
            } else if name == "logs.json" {
                2
            } else if name.contains("chat") || name.contains("session") {
                1
            } else {
                0
            };
            let meta = std::fs::metadata(&path).ok();
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let size = meta.map(|m| m.len()).unwrap_or(0);
            let candidate = path.to_string_lossy().to_string();
            let better = match best {
                None => true,
                Some((br, bm, bs, _)) => (rank, mtime, size) > (*br, *bm, *bs),
            };
            if better {
                *best = Some((rank, mtime, size, candidate));
            }
        }
    }
}

/// Looks up a herdr agent's workspace label/number and tab label from the id maps built
/// from `herdr workspace list` / `herdr tab list`.
fn herdr_workspace_fields(
    h: &HerdrAgent,
    workspaces: &HashMap<String, (String, i64)>,
    tabs: &HashMap<String, (String, i64)>,
) -> (Option<String>, Option<i64>, Option<String>, Option<i64>) {
    let workspace_entry = h.workspace_id.as_deref().and_then(|id| workspaces.get(id));
    let workspace_label = workspace_entry.map(|(l, _)| l.clone());
    let workspace_order = workspace_entry.map(|(_, n)| *n);
    let tab_entry = h.tab_id.as_deref().and_then(|id| tabs.get(id));
    let tab_label = tab_entry.map(|(l, _)| l.clone());
    let tab_order = tab_entry.map(|(_, n)| *n);
    (workspace_label, workspace_order, tab_label, tab_order)
}

/// Merges registry sessions and herdr agents into the SessionView contract.
/// A registry session and a herdr agent are matched into source="both" when the
/// registry pid equals the pane's foreground process group id — an exact 1:1 join
/// (the claude process IS the pane's foreground process group leader).
fn build_sessions(
    registry: Vec<RegistryEntry>,
    herdr: Vec<HerdrAgent>,
    workspaces: &HashMap<String, (String, i64)>,
    tabs: &HashMap<String, (String, i64)>,
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
            let (workspace, workspace_order, tab_label, tab_order) =
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
                workspace_order,
                tab_id: h.tab_id.clone(),
                tab_label,
                tab_order,
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
                workspace_order: None,
                tab_id: None,
                tab_label: None,
                tab_order: None,
                branch: git_branch(&r.cwd),
            });
        }
    }

    for h in &herdr {
        if matched_panes.contains(h.pane_id.as_str()) {
            continue;
        }
        let (workspace, workspace_order, tab_label, tab_order) =
            herdr_workspace_fields(h, workspaces, tabs);
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
            transcript_path: match h.agent.as_str() {
                "codex" => find_codex_transcript(&h.cwd),
                "gemini" => find_gemini_transcript(&h.cwd),
                _ => None,
            },
            updated_at: None,
            display_name: resolve_display_name(
                workspace.as_deref(),
                tab_label.as_deref(),
                None,
                &h.cwd,
            ),
            workspace,
            workspace_id: h.workspace_id.clone(),
            workspace_order,
            tab_id: h.tab_id.clone(),
            tab_label,
            tab_order,
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
    ext: &str,
) -> Result<PathBuf, String> {
    if path.extension().and_then(|value| value.to_str()) != Some(ext) {
        return Err("트랜스크립트 파일 형식이 올바르지 않습니다".to_string());
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

/// Validates a transcript path against the allowed roots and returns (canonical path, format).
/// Claude/Codex use JSONL; Gemini uses .json (older CLI) or .jsonl (current) — both listed.
fn validated_transcript(path: &str) -> Result<(PathBuf, &'static str), String> {
    let home = std::env::var("HOME").map_err(|_| "HOME 경로를 찾을 수 없습니다".to_string())?;
    let roots: [(&str, &str, &str); 4] = [
        (".claude/projects", "jsonl", "claude"),
        (".codex/sessions", "jsonl", "codex"),
        (".gemini/tmp", "json", "gemini"),
        (".gemini/tmp", "jsonl", "gemini"),
    ];
    let mut last_err = "허용되지 않은 트랜스크립트 경로입니다".to_string();
    for (rel, ext, format) in roots {
        let root = PathBuf::from(&home).join(rel);
        match validate_transcript_path_with_root(std::path::Path::new(path), &root, ext) {
            Ok(canonical) => return Ok((canonical, format)),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
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
        || trimmed.starts_with("<session_context")
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

/// Concatenates the text blocks of a codex `message` item's content array.
fn codex_content_text(content: Option<&Value>) -> String {
    let arr = match content.and_then(Value::as_array) {
        Some(a) => a,
        None => return content.and_then(Value::as_str).unwrap_or("").to_string(),
    };
    let mut out = String::new();
    for block in arr {
        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(block_type, "input_text" | "output_text" | "text" | "") {
            if let Some(s) = block.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(s);
            }
        }
    }
    out
}

/// Summarizes a codex function_call's arguments (a JSON string) to its most identifying value.
fn codex_tool_text(args: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(args) {
        for key in ["command", "cmd", "file_path", "path", "query", "pattern", "url"] {
            if let Some(s) = v.get(key).and_then(Value::as_str) {
                return truncate(s, 360);
            }
        }
        if let Some(arr) = v.get("command").and_then(Value::as_array) {
            let joined = arr
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            if !joined.is_empty() {
                return truncate(&joined, 360);
            }
        }
    }
    truncate(args, 360)
}

/// Parses a codex rollout JSONL line into timeline items. Defensive: handles both the
/// response_item (message/function_call) and event_msg (agent_message/user_message) shapes,
/// wrapped under "payload" or not, and skips anything else.
fn parse_codex_line(line: &str) -> Vec<TimelineItem> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let ts = v
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    let inner = v.get("payload").unwrap_or(&v);
    let kind = inner.get("type").and_then(Value::as_str).unwrap_or("");
    let mut items = Vec::new();
    match kind {
        "message" => {
            let role = inner.get("role").and_then(Value::as_str).unwrap_or("");
            let text = codex_content_text(inner.get("content"));
            if !text.is_empty() && (role == "user" || role == "assistant") {
                if role == "user" && ignored_user_text(&text) {
                    return items;
                }
                items.push(TimelineItem {
                    role: role.to_string(),
                    text: truncate(&text, 12_000),
                    ts,
                    tool_name: None,
                });
            }
        }
        "function_call" => {
            let name = inner.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = inner.get("arguments").and_then(Value::as_str).unwrap_or("");
            items.push(TimelineItem {
                role: "tool".to_string(),
                text: codex_tool_text(args),
                ts,
                tool_name: Some(name.to_string()),
            });
        }
        "agent_message" => {
            if let Some(t) = inner.get("message").and_then(Value::as_str) {
                if !t.is_empty() {
                    items.push(TimelineItem {
                        role: "assistant".to_string(),
                        text: truncate(t, 12_000),
                        ts,
                        tool_name: None,
                    });
                }
            }
        }
        "user_message" => {
            if let Some(t) = inner.get("message").and_then(Value::as_str) {
                if !t.is_empty() && !ignored_user_text(t) {
                    items.push(TimelineItem {
                        role: "user".to_string(),
                        text: truncate(t, 8_000),
                        ts,
                        tool_name: None,
                    });
                }
            }
        }
        _ => {}
    }
    items
}

/// The messages of a gemini conversation file, in order. Three on-disk shapes:
///   - logs.json — a bare array of {type, message} (user prompts only)
///   - chats/*.json (older CLI) — one document, {messages: [...]}
///   - chats/*.jsonl (current CLI) — an op log: a bare object appends one message,
///     {"$set": {"messages": [...]}} replaces the whole array. Replaying both in file order
///     lands on the final state.
fn gemini_messages(content: &str) -> Vec<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(content) {
        if let Some(arr) = v.as_array() {
            return arr.clone();
        }
        if let Some(arr) = v.get("messages").and_then(Value::as_array) {
            return arr.clone();
        }
        return Vec::new();
    }
    let mut messages: Vec<Value> = Vec::new();
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(set) = v.get("$set") {
            if let Some(arr) = set.get("messages").and_then(Value::as_array) {
                messages = arr.clone();
            }
        } else if v.get("type").is_some() {
            messages.push(v);
        }
    }
    messages
}

/// Message text, minus the model's internal monologue: gemini splits `content` into parts and marks
/// reasoning ones `thought: true` — keeping those would show thinking instead of the answer.
fn gemini_text(message: &Value) -> String {
    if let Some(s) = message.get("message").and_then(Value::as_str) {
        return s.to_string();
    }
    let content = message.get("content").unwrap_or(&Value::Null);
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    parts
        .iter()
        .filter(|p| p.get("thought") != Some(&Value::Bool(true)))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses a gemini conversation file into timeline items. A message is {type, content, toolCalls},
/// where type is "user" | "gemini" (plus "info"/"error" chatter we drop), and a user turn carrying
/// only functionResponse parts is a tool result, not something the user said.
fn parse_gemini(content: &str) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    for message in gemini_messages(content) {
        let role = match message.get("type").and_then(Value::as_str) {
            Some("user") => "user",
            Some("gemini") | Some("assistant") | Some("model") => "assistant",
            _ => continue,
        };
        let text = gemini_text(&message);
        let ts = message
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        let hidden = text.is_empty() || (role == "user" && ignored_user_text(&text));
        if !hidden {
            items.push(TimelineItem {
                role: role.to_string(),
                text: truncate(&text, 12_000),
                ts: ts.clone(),
                tool_name: None,
            });
        }
        for call in message
            .get("toolCalls")
            .and_then(Value::as_array)
            .unwrap_or(&Vec::new())
        {
            let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = call
                .get("args")
                .map(|a| serde_json::to_string(a).unwrap_or_default())
                .unwrap_or_default();
            items.push(TimelineItem {
                role: "tool".to_string(),
                text: truncate(&args, 360),
                ts: ts.clone(),
                tool_name: Some(name.to_string()),
            });
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

/// Claude stamps the model on every assistant line, so the last one is what the session runs on
/// now — `/model` mid-session simply changes the lines after it. `<synthetic>` marks Claude Code's
/// own error messages, not a model.
fn claude_model(text: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let value: Value = serde_json::from_str(line).ok()?;
        match value["message"]["model"].as_str() {
            Some("<synthetic>") | None => None,
            Some(model) => Some(model.to_string()),
        }
    })
}

/// Codex re-emits a `turn_context` line every turn; the last one carries the model and reasoning
/// effort in force right now.
fn codex_model(text: &str) -> (Option<String>, Option<String>) {
    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["type"] != "turn_context" {
            continue;
        }
        let payload = &value["payload"];
        return (
            payload["model"].as_str().map(str::to_string),
            payload["effort"].as_str().map(str::to_string),
        );
    }
    (None, None)
}

#[tauri::command]
fn get_timeline(transcript_path: String, last: usize) -> Result<Timeline, String> {
    let (path, format) = validated_transcript(&transcript_path)?;
    let path_str = path.to_string_lossy().to_string();
    // Model/effort come out of the same bytes the timeline is parsed from, so they stay in sync
    // with a mid-session /model without a second read. Gemini's log records neither.
    let (mut items, model, effort) = match format {
        "gemini" => {
            let content = std::fs::read_to_string(&path_str).map_err(|e| e.to_string())?;
            (parse_gemini(&content), None, None)
        }
        "codex" => {
            let text = read_tail(&path_str)?;
            let items = text.lines().flat_map(parse_codex_line).collect();
            let (model, effort) = codex_model(&text);
            (items, model, effort)
        }
        _ => {
            let text = read_tail(&path_str)?;
            let items = text.lines().flat_map(parse_line).collect();
            (items, claude_model(&text), None)
        }
    };
    let start = items.len().saturating_sub(last);
    items = items.split_off(start);
    Ok(Timeline {
        items,
        model,
        effort,
    })
}

/// Transcript mtime in epoch millis (0 if missing) — a cheap change check so the UI can
/// poll this instead of re-parsing the whole transcript every second.
#[tauri::command]
fn get_timeline_revision(transcript_path: String) -> Result<i64, String> {
    let (transcript_path, _) = validated_transcript(&transcript_path)?;
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
    let pane_num = match pane_part.strip_prefix('p') {
        Some(d) => d,
        None => return Err("잘못된 pane id".to_string()),
    };
    // herdr numbers panes in base36-ish ids (e.g. "wA:pA"), so allow alphanumerics — still no
    // shell-dangerous chars, spaces, or leading dashes, which is what this trust check guards.
    if pane_num.is_empty() || !pane_num.chars().all(|c| c.is_ascii_alphanumeric()) {
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

/// herdr accepts any key name here (ctrl+c, esc, letters…). We don't: the choice buttons only ever
/// need a digit or a cursor move, and the labels they are built from come from agent-rendered
/// screen text, which is adversarial input.
fn key_allowed(key: &str) -> bool {
    matches!(key, "up" | "down" | "enter" | "esc")
        || matches!(key.as_bytes(), [b'1'..=b'9'])
}

#[tauri::command]
fn send_keys(pane_id: String, keys: Vec<String>) -> Result<(), String> {
    validate_pane_id(&pane_id)?;
    if keys.is_empty() || keys.len() > 4 {
        return Err("키 개수가 올바르지 않습니다".to_string());
    }
    if let Some(bad) = keys.iter().find(|k| !key_allowed(k)) {
        return Err(format!("허용되지 않은 키: {bad}"));
    }
    if !read_herdr_snapshot()
        .agents
        .iter()
        .any(|a| a.pane_id == pane_id)
    {
        return Err("이 세션의 pane이 더 이상 존재하지 않습니다".to_string());
    }

    event_stream::send_keys(&pane_id, &keys)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri::{Emitter, Manager};

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            list_sessions,
            get_timeline,
            get_timeline_revision,
            read_pane,
            send_message,
            send_keys,
            detect_providers,
            event_stream::herdr_event_status
        ])
        .on_menu_event(|app, event| {
            // Native "Settings…" (Cmd+,) just tells the webview to open its settings panel.
            if event.id().as_ref() == "settings" {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                    let _ = window.emit("open-settings", ());
                }
            }
        })
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
            use tauri::tray::TrayIconBuilder;

            event_stream::start(app.handle().clone());

            // macOS application menu: the app-name submenu carries About, Settings… (Cmd+,), Quit.
            let settings_item =
                MenuItem::with_id(app, "settings", "Settings…", true, Some("CmdOrCtrl+,"))?;
            let app_menu = Submenu::with_items(
                app,
                "HerdDeck",
                true,
                &[
                    &PredefinedMenuItem::about(app, None, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &settings_item,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?;
            let edit_menu = Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?;
            let menu_bar = Menu::with_items(app, &[&app_menu, &edit_menu])?;
            app.set_menu(menu_bar)?;

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
    fn claude_model_takes_the_last_real_model() {
        let text = [
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5","role":"assistant"}}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","role":"assistant"}}"#,
            // Claude Code's own error lines are stamped <synthetic> — not a model.
            r#"{"type":"assistant","message":{"model":"<synthetic>","role":"assistant"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
        ]
        .join("\n");
        assert_eq!(claude_model(&text).as_deref(), Some("claude-opus-4-8"));
        assert_eq!(claude_model(r#"{"type":"user"}"#), None);
    }

    #[test]
    fn codex_model_takes_the_last_turn_context() {
        let text = [
            r#"{"type":"turn_context","payload":{"model":"gpt-5.6","effort":"low"}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5.6-sol","effort":"medium"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]
        .join("\n");
        let (model, effort) = codex_model(&text);
        assert_eq!(model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(effort.as_deref(), Some("medium"));
        assert_eq!(codex_model(r#"{"type":"event_msg"}"#), (None, None));
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
        assert!(validate_pane_id("wA:pA").is_ok());
        assert!(validate_pane_id("w9:p12").is_ok());
        assert!(validate_pane_id("wAbc123:p0").is_ok());
    }

    #[test]
    fn rejects_invalid_pane_ids() {
        for bad in [
            "p3",
            "wA:p",
            "wA:3",
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
    fn only_choice_digits_and_prompt_navigation_are_sendable_keys() {
        for good in ["1", "9", "up", "down", "enter", "esc"] {
            assert!(key_allowed(good), "expected {good:?} to be allowed");
        }
        // "0" is never a choice number, and the rest would let screen-derived buttons
        // interrupt, background, or type into a live agent.
        for bad in ["0", "10", "", "ctrl+c", "ctrl+z", "tab", "y", "q", "escape"] {
            assert!(!key_allowed(bad), "expected {bad:?} to be rejected");
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
            validate_transcript_path_with_root(&inside, &root, "jsonl").unwrap(),
            std::fs::canonicalize(&inside).unwrap()
        );
        assert!(validate_transcript_path_with_root(&outside, &root, "jsonl").is_err());
        assert!(validate_transcript_path_with_root(&wrong_extension, &root, "jsonl").is_err());

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
    fn parses_gemini_jsonl_op_log() {
        // 현재 Gemini CLI 포맷: 헤더 + 메시지 append + {"$set":{"messages":[...]}} 전체 교체
        let content = concat!(
            r#"{"sessionId":"c20","startTime":"2026-07-13T09:50:32.971Z","kind":"main"}"#,
            "\n",
            r#"{"id":"m0","timestamp":"2026-07-13T09:51:00.000Z","type":"info","content":"Gemini CLI update available!"}"#,
            "\n",
            r#"{"$set":{"messages":[{"id":"m1","timestamp":"2026-07-13T09:52:00.000Z","type":"user","content":[{"text":"<session_context>\nThis is the Gemini CLI."}]},{"id":"m2","timestamp":"2026-07-13T09:53:00.000Z","type":"user","content":[{"text":"이 프로젝트를 보고 이름을 뭐로 할지 고민좀"}]},{"id":"m3","timestamp":"2026-07-13T09:54:00.000Z","type":"user","content":[{"functionResponse":{"name":"read_file"}}]},{"id":"m4","timestamp":"2026-07-13T09:55:00.000Z","type":"gemini","content":[{"text":"**Considering Project Identity** I'm currently...","thought":true},{"text":"이름은 HerdDeck 을 추천합니다."}],"toolCalls":[{"name":"read_file","args":{"path":"src/lib.rs"}}]}]}}"#
        );

        let items = parse_gemini(content);

        // 사용자 프롬프트 1 + 모델 답변 1 + 툴 1 — info/session_context/functionResponse-only 는 제외
        let said: Vec<_> = items.iter().filter(|i| i.role != "tool").collect();
        assert_eq!(said.len(), 2, "대화가 비어있음: {items:?}");
        assert_eq!(said[0].role, "user");
        assert_eq!(said[0].text, "이 프로젝트를 보고 이름을 뭐로 할지 고민좀");
        assert_eq!(said[1].role, "assistant");
        // thought 파트가 아니라 실제 답변이 나와야 함
        assert_eq!(said[1].text, "이름은 HerdDeck 을 추천합니다.");
        assert_eq!(
            items.iter().filter(|i| i.role == "tool").count(),
            1,
            "toolCalls 누락"
        );
    }

    #[test]
    fn parses_gemini_legacy_json_document() {
        // 구버전 CLI: chats/*.json 단일 문서, gemini content 가 문자열
        let content = r#"{"sessionId":"56e","messages":[
            {"id":"1","timestamp":"2026-02-26T04:15:00.000Z","type":"user","content":[{"text":"두 API 구현 차이 있나"}]},
            {"id":"2","timestamp":"2026-02-26T04:16:00.000Z","type":"gemini","content":"추상화 계층부터 확인하겠습니다."}
        ]}"#;

        let items = parse_gemini(content);

        assert_eq!(items.len(), 2, "구포맷이 깨짐: {items:?}");
        assert_eq!(items[0].role, "user");
        assert_eq!(items[1].role, "assistant");
        assert_eq!(items[1].text, "추상화 계층부터 확인하겠습니다.");
    }

    #[test]
    fn gemini_scan_prefers_live_chat_over_same_mtime_stub() {
        // 세션 재개가 남긴 스텁은 실제 대화 파일과 mtime 이 같다 — 크기로 갈라야 한다
        let dir = std::env::temp_dir().join("herddeck-gemini-scan-test/chats");
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("session-2026-07-13T09-50-c20bbcd8.jsonl");
        let stub = dir.join("session-2026-07-13T12-30-c20bbcd8.jsonl");
        std::fs::write(&live, "x".repeat(200_000)).unwrap();
        std::fs::write(&stub, "x".repeat(2_909)).unwrap();
        let mtime = std::fs::metadata(&live).unwrap().modified().unwrap();
        for path in [&live, &stub] {
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(mtime).unwrap();
        }

        let mut best = None;
        gemini_scan_dir(dir.parent().unwrap(), &mut best);

        assert_eq!(best.map(|(_, _, _, p)| p), Some(live.to_string_lossy().to_string()));
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
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
