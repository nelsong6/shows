use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};
use tiny_http::{Response, Server};

fn run(args: &[&str], replies: Vec<(&'static str, Value)>) -> std::process::Output {
    let server = Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let worker = std::thread::spawn(move || {
        for (path, value) in replies {
            let request = server
                .recv_timeout(Duration::from_secs(4))
                .unwrap()
                .expect("request");
            assert_eq!(request.url(), path);
            request
                .respond(Response::from_string(value.to_string()))
                .unwrap();
        }
    });
    let result = Command::new(env!("CARGO_BIN_EXE_shows-remote"))
        .env("SHOWS_CONTROL_PORT", port.to_string())
        .arg("--local")
        .arg("thispc")
        .args(args)
        .output()
        .unwrap();
    worker.join().unwrap();
    result
}

fn identity() -> (&'static str, Value) {
    (
        "/remote/info",
        json!({"application":"shows-desktop","protocol":1}),
    )
}

#[test]
fn local_agent_round_trip_prints_json_without_starting_a_player() {
    let output = run(
        &["status", "--json"],
        vec![
            identity(),
            ("/status", json!({"phase":"playing","message":"Télévision"})),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["message"], "Télévision");
}

#[test]
fn rejected_control_exits_nonzero_and_surfaces_the_target_error() {
    let output = run(
        &["play"],
        vec![
            identity(),
            (
                "/remote/control",
                json!({"ok":false,"message":"player unavailable"}),
            ),
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("player unavailable"));
}

#[test]
fn fullscreen_uses_the_native_control_route() {
    let output = run(
        &["fullscreen"],
        vec![
            identity(),
            (
                "/remote/control",
                json!({"ok":true,"message":"fullscreen toggle requested"}),
            ),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("fullscreen toggle requested"));
}

#[test]
fn ambiguous_show_selection_sends_no_play_request() {
    let output = run(
        &["play", "the"],
        vec![
            identity(),
            (
                "/status",
                json!({"round":[
        {"show_id":"a","show_name":"The Simpsons"}, {"show_id":"b","show_name":"The Daily Show"}]}),
            ),
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("several"));
}

#[test]
fn scripts_get_a_clear_error_instead_of_entering_raw_terminal_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_shows-remote"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("needs a terminal"));
}

#[test]
fn one_shot_commands_never_pick_an_implicit_machine() {
    let output = Command::new(env!("CARGO_BIN_EXE_shows-remote"))
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Commands require a target"));
}

#[cfg(windows)]
#[test]
fn installed_windows_agent_inherits_protocol_pipes_through_the_remote_entry_point() {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("shows");
    let version = root.join("versions").join("smoke");
    std::fs::create_dir_all(&version).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_shows-remote"),
        version.join("shows-remote.exe"),
    )
    .unwrap();
    std::fs::write(
        root.join("launcher-state-stable.json"),
        r#"{"current":"smoke"}"#,
    )
    .unwrap();
    std::fs::create_dir(root.join("agents")).unwrap();
    std::fs::write(
        root.join("agents").join("stable.cmd"),
        b"@\"%~dp0..\\versions\\smoke\\shows-remote.exe\" --agent\r\n",
    )
    .unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let worker = std::thread::spawn(move || {
        for value in [
            identity().1,
            json!({"phase":"playing","message":"Télévision"}),
        ] {
            server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .expect("agent HTTP request")
                .respond(Response::from_string(value.to_string()))
                .unwrap();
        }
    });
    let mut child = Command::new("cmd.exe")
        .args(["/d", "/s", "/c"])
        .raw_arg(format!(
            "\"{}\"",
            shows_remote::transport::remote_command("stable")
        ))
        .env("LOCALAPPDATA", fixture.path())
        .env("SHOWS_CONTROL_PORT", port.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"id\":1,\"command\":{\"action\":\"status\"}}\n")
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let timed_out = child.try_wait().unwrap().is_none();
    if timed_out {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success() && !timed_out,
        "timeout={timed_out}; stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    worker.join().unwrap();
    let frame: shows_control::Frame = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        matches!(frame, shows_control::Frame::Reply { result: Ok(status), .. } if status["message"] == "Télévision")
    );
}
