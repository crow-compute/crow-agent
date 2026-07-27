use crow_agent_protocol::HARNESS_PROTOCOL_V1;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentStatus {
    protocol: &'static str,
    execution_boundary: &'static str,
    daemon: &'static str,
    active_run: Option<String>,
}

#[tauri::command]
fn get_agent_status() -> AgentStatus {
    AgentStatus {
        protocol: HARNESS_PROTOCOL_V1,
        execution_boundary: "local_device",
        daemon: "stopped",
        active_run: None,
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_agent_status])
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| {
            eprintln!("Crow Agent desktop runtime failed: {error}");
            std::process::exit(1);
        });
}
