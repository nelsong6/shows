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
