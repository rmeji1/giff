use serde_json::Value;
use std::error::Error;

const SESSIONS_URL: &str = "http://localhost:4747/api/sessions";
const INPUT_URL_BASE: &str = "http://localhost:4748/input";

/// Find a Bourne Board session whose directory matches the given path.
/// Returns the session ID if found.
pub fn find_session(directory: &str) -> Result<Option<String>, Box<dyn Error>> {
    let resp = ureq::get(SESSIONS_URL).call().map_err(|e| match e {
        ureq::Error::Transport(_) => "Bourne Board not running".into(),
        other => Box::<dyn Error>::from(other.to_string()),
    })?;

    let body_str = resp.into_string()?;
    let body: Value = serde_json::from_str(&body_str)?;
    let sessions = body.as_array().ok_or("Unexpected sessions response")?;

    for session in sessions {
        if let Some(dir) = session.get("directory").and_then(Value::as_str) {
            if dir == directory {
                if let Some(id) = session.get("id").and_then(Value::as_str) {
                    return Ok(Some(id.to_string()));
                }
            }
        }
    }

    Ok(None)
}

/// Send text to a Bourne Board session's PTY input.
fn send_to_session(session_id: &str, text: &str) -> Result<(), Box<dyn Error>> {
    let url = format!("{}/{}", INPUT_URL_BASE, session_id);
    let payload = serde_json::json!({ "text": text });

    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .map_err(|e| format!("Failed to send comment: {}", e))?;

    if resp.status() == 404 {
        return Err("Session PTY not running".into());
    }

    Ok(())
}

/// Send a comment to the Claude Code session matching the given repo directory.
pub fn send_comment(directory: &str, text: &str) -> Result<(), Box<dyn Error>> {
    let session_id = find_session(directory)?
        .ok_or("No Claude Code session found for this directory")?;
    send_to_session(&session_id, text)
}
