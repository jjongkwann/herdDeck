use serde::Serialize;
#[cfg(unix)]
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, OnceLock,
};
#[cfg(unix)]
use std::{
    collections::HashSet,
    env,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    thread,
    time::Duration,
};
use tauri::{AppHandle, Emitter};

static CONNECTED: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventStreamStatus {
    connected: bool,
    error: Option<String>,
}

fn last_error() -> &'static Mutex<Option<String>> {
    LAST_ERROR.get_or_init(|| Mutex::new(None))
}

fn set_status(connected: bool, error: Option<String>) {
    CONNECTED.store(connected, Ordering::Relaxed);
    if let Ok(mut current) = last_error().lock() {
        *current = error;
    }
}

#[tauri::command]
pub fn herdr_event_status() -> EventStreamStatus {
    EventStreamStatus {
        connected: CONNECTED.load(Ordering::Relaxed),
        error: last_error().lock().ok().and_then(|error| error.clone()),
    }
}

#[cfg(unix)]
fn socket_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("HERDR_SOCKET_PATH") {
        return Ok(PathBuf::from(path));
    }

    if let Some(config) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config).join("herdr/herdr.sock"));
    }

    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config/herdr/herdr.sock"))
        .ok_or_else(|| "Herdr socket 경로를 찾을 수 없습니다.".to_owned())
}

#[cfg(unix)]
fn write_request(stream: &mut UnixStream, value: &Value) -> Result<(), String> {
    let mut payload = serde_json::to_vec(value)
        .map_err(|error| format!("Herdr 요청을 만들 수 없습니다: {error}"))?;
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .map_err(|error| format!("Herdr socket에 쓸 수 없습니다: {error}"))
}

#[cfg(unix)]
fn read_json_line(reader: &mut BufReader<UnixStream>) -> Result<Value, String> {
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .map_err(|error| format!("Herdr socket을 읽을 수 없습니다: {error}"))?;
    if read == 0 {
        return Err("Herdr가 연결을 종료했습니다.".to_owned());
    }
    serde_json::from_str(&line)
        .map_err(|error| format!("Herdr 이벤트를 해석할 수 없습니다: {error}"))
}

#[cfg(unix)]
fn send_input_request(pane_id: &str, text: &str) -> Value {
    json!({
        "id": "remote-legion:send-input",
        "method": "pane.send_input",
        "params": { "pane_id": pane_id, "text": text, "keys": ["enter"] }
    })
}

/// Sends text and Enter in one server request. Protocol 16's pane.send_input applies
/// `text` and `keys` together, avoiding the timing race between two CLI subprocesses.
#[cfg(unix)]
pub fn send_text_and_enter(pane_id: &str, text: &str) -> Result<(), String> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path)
        .map_err(|error| format!("Herdr socket 연결 실패 ({}): {error}", path.display()))?;
    let reader_stream = stream
        .try_clone()
        .map_err(|error| format!("Herdr socket 복제 실패: {error}"))?;
    let mut reader = BufReader::new(reader_stream);

    write_request(&mut stream, &send_input_request(pane_id, text))?;
    let response = read_json_line(&mut reader)?;
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Herdr가 입력을 거부했습니다");
        return Err(message.to_string());
    }
    if response.get("result").is_none() {
        return Err("Herdr 입력 응답 형식이 올바르지 않습니다".to_string());
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn send_text_and_enter(_pane_id: &str, _text: &str) -> Result<(), String> {
    Err("이 플랫폼의 Herdr 입력은 아직 지원되지 않습니다".to_string())
}

#[cfg(unix)]
fn structural_subscriptions() -> Vec<Value> {
    [
        "workspace.created",
        "workspace.renamed",
        "workspace.closed",
        "tab.created",
        "tab.closed",
        "tab.renamed",
        "pane.created",
        "pane.closed",
        "pane.exited",
        "pane.agent_detected",
    ]
    .into_iter()
    .map(|event_type| json!({ "type": event_type }))
    .collect()
}

#[cfg(unix)]
fn needs_status_resubscribe(event: &Value, subscribed_panes: &HashSet<String>) -> bool {
    event.get("event").and_then(Value::as_str) == Some("pane.agent_detected")
        && event
            .pointer("/data/pane_id")
            .and_then(Value::as_str)
            .is_some_and(|pane_id| !subscribed_panes.contains(pane_id))
}

#[cfg(unix)]
fn subscribe_once(app: &AppHandle) -> Result<(), String> {
    let path = socket_path()?;
    let mut snapshot_stream = UnixStream::connect(&path)
        .map_err(|error| format!("Herdr socket 연결 실패 ({}): {error}", path.display()))?;
    let snapshot_reader_stream = snapshot_stream
        .try_clone()
        .map_err(|error| format!("Herdr socket 복제 실패: {error}"))?;
    let mut snapshot_reader = BufReader::new(snapshot_reader_stream);

    write_request(
        &mut snapshot_stream,
        &json!({ "id": "remote-legion:snapshot", "method": "session.snapshot", "params": {} }),
    )?;
    let snapshot = read_json_line(&mut snapshot_reader)?;
    let pane_ids: Vec<String> = snapshot
        .pointer("/result/snapshot/agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|agent| agent.get("pane_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    drop(snapshot_reader);
    drop(snapshot_stream);

    let subscribed_panes: HashSet<String> = pane_ids.iter().cloned().collect();
    let mut subscriptions = structural_subscriptions();
    for pane_id in pane_ids {
        subscriptions.push(json!({
            "type": "pane.agent_status_changed",
            "pane_id": pane_id
        }));
    }

    let mut stream = UnixStream::connect(&path)
        .map_err(|error| format!("Herdr 구독 socket 연결 실패 ({}): {error}", path.display()))?;
    let reader_stream = stream
        .try_clone()
        .map_err(|error| format!("Herdr 구독 socket 복제 실패: {error}"))?;
    let mut reader = BufReader::new(reader_stream);

    write_request(
        &mut stream,
        &json!({
            "id": "remote-legion:subscribe",
            "method": "events.subscribe",
            "params": { "subscriptions": subscriptions }
        }),
    )?;
    let acknowledgement = read_json_line(&mut reader)?;
    if acknowledgement
        .pointer("/result/type")
        .and_then(Value::as_str)
        != Some("subscription_started")
    {
        return Err("Herdr가 이벤트 구독을 승인하지 않았습니다.".to_owned());
    }

    let _ = app.emit(
        "herdr-connection",
        json!({ "connected": true, "transport": "socket" }),
    );
    set_status(true, None);

    loop {
        let event = read_json_line(&mut reader)?;
        let _ = app.emit("herdr-event", &event);
        // Status subscriptions are pane-scoped by protocol 16. Reconnect with a fresh
        // snapshot when a newly detected pane was not part of the initial subscription.
        // Existing pane detection events are ignored, preventing replay/reconnect loops.
        if needs_status_resubscribe(&event, &subscribed_panes) {
            return Ok(());
        }
    }
}

pub fn start(app: AppHandle) {
    #[cfg(unix)]
    thread::spawn(move || loop {
        if let Err(error) = subscribe_once(&app) {
            eprintln!("remote-legion herdr event stream: {error}");
            set_status(false, Some(error.clone()));
            let _ = app.emit(
                "herdr-connection",
                json!({ "connected": false, "transport": "socket", "error": error }),
            );
            thread::sleep(Duration::from_secs(2));
        } else {
            // Planned resubscribe after pane.agent_detected. Keep the connection state
            // green and immediately rebuild pane-scoped status subscriptions.
            thread::sleep(Duration::from_millis(50));
        }
    });

    #[cfg(not(unix))]
    {
        let _ = app.emit(
            "herdr-connection",
            serde_json::json!({
                "connected": false,
                "transport": "socket",
                "error": "이 플랫폼의 Herdr socket 연결은 아직 지원되지 않습니다."
            }),
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn resubscribes_only_for_newly_detected_panes() {
        let subscribed = HashSet::from(["w1:p1".to_string()]);
        let known = json!({"event":"pane.agent_detected","data":{"pane_id":"w1:p1"}});
        let new = json!({"event":"pane.agent_detected","data":{"pane_id":"w1:p2"}});
        let unrelated = json!({"event":"pane.created","data":{"pane_id":"w1:p2"}});

        assert!(!needs_status_resubscribe(&known, &subscribed));
        assert!(needs_status_resubscribe(&new, &subscribed));
        assert!(!needs_status_resubscribe(&unrelated, &subscribed));
    }

    #[test]
    fn send_input_request_combines_text_and_enter() {
        let request = send_input_request("w1:p2", "진행해줘");
        assert_eq!(request["method"], "pane.send_input");
        assert_eq!(request["params"]["pane_id"], "w1:p2");
        assert_eq!(request["params"]["text"], "진행해줘");
        assert_eq!(request["params"]["keys"], json!(["enter"]));
    }
}
