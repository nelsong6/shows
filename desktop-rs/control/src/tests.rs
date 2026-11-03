use super::*;
use std::io::Cursor;
use tiny_http::{Header, Response, Server};

#[test]
fn live_overlay_is_explicit_and_cannot_replace_release_assets() {
    assert_eq!(
        overlay_url(45831, None, false).unwrap(),
        "http://127.0.0.1:45831/"
    );
    assert!(overlay_url(45831, Some("http://127.0.0.1:5173/"), false).is_err());
    assert!(overlay_url(45831, None, true).is_err());
    assert_eq!(
        overlay_url(45831, Some("http://127.0.0.1:5173/"), true).unwrap(),
        "http://127.0.0.1:5173/"
    );
    for url in [
        "https://example.com/",
        "http://upstairspc/",
        "javascript:alert(1)",
        "http://user@localhost/",
    ] {
        assert!(overlay_url(45831, Some(url), true).is_err());
    }
}

fn fixture(replies: Vec<(&'static str, u16, Value)>) -> (String, std::thread::JoinHandle<()>) {
    let server = Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_string();
    let thread = std::thread::spawn(move || {
        for (path, code, value) in replies {
            let mut request = server
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .expect("request");
            assert_eq!(request.url(), path);
            if path == "/remote/control" {
                assert_eq!(request.method(), &tiny_http::Method::Post);
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let command: Command = serde_json::from_str(&body).unwrap();
                assert!(matches!(command, Command::Play {} | Command::Pause {}));
            }
            request
                .respond(Response::from_string(value.to_string()).with_status_code(code))
                .unwrap();
        }
        assert!(
            server
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none(),
            "request was replayed"
        );
    });
    (address, thread)
}

fn identity() -> Value {
    json!({"application":"shows-desktop","protocol":PROTOCOL_VERSION})
}

#[test]
fn paused_core_is_controllable_but_an_unloaded_episode_is_not() {
    let mut status = json!({"phase":"playing","round_pos":0,"round":[{"episode_id":"e1"}],
        "playback":{"time_pos":20,"paused":true,"core_idle":true}});
    assert!(playback_ready(&status));
    status["playback"]["time_pos"] = Value::Null;
    assert!(!playback_ready(&status));
}

#[test]
fn rejects_network_http_and_wrong_protocol_before_sending_controls() {
    assert!(
        Client::connect_to("192.0.2.1:45831")
            .err()
            .unwrap()
            .contains("loopback")
    );
    for value in [
        json!({"application":"other","protocol":1}),
        json!({"application":"shows-desktop","protocol":99}),
    ] {
        let (address, worker) = fixture(vec![("/remote/info", 200, value)]);
        assert!(Client::connect_to(&address).is_err());
        worker.join().unwrap();
    }
}

#[test]
fn controls_report_target_rejection_and_never_replay_posts_or_redirects() {
    for (code, result) in [
        (503, json!({"ok":false,"message":"no episode loaded"})),
        (200, json!({"ok":false,"message":"show not in round"})),
        (307, json!({"message":"redirect refused"})),
    ] {
        let (address, worker) = fixture(vec![
            ("/remote/info", 200, identity()),
            ("/remote/control", code, result.clone()),
        ]);
        let client = Client::connect_to(&address).unwrap();
        assert!(
            client
                .execute(&Command::Play {})
                .unwrap_err()
                .contains(result["message"].as_str().unwrap())
        );
        worker.join().unwrap();
    }
}

#[test]
fn invalid_numeric_controls_are_rejected_before_network_io() {
    let (address, worker) = fixture(vec![("/remote/info", 200, identity())]);
    let client = Client::connect_to(&address).unwrap();
    for command in [
        Command::Volume { level: 131.0 },
        Command::Volume { level: f64::NAN },
        Command::Seek {
            seconds: f64::INFINITY,
        },
    ] {
        assert!(client.execute(&command).is_err());
    }
    worker.join().unwrap();
    assert!(serde_json::from_value::<Command>(json!({"action":"skip","extra":true})).is_err());
}

#[test]
fn show_selection_is_unique_and_uses_only_the_target_round() {
    let status = json!({"round":[{"show_id":"1","show_name":"The Simpsons"},
        {"show_id":"2","show_name":"The Daily Show"}]});
    assert_eq!(resolve_show(&status, "simpsons").unwrap(), "1");
    assert_eq!(resolve_show(&status, "2").unwrap(), "2");
    assert!(
        resolve_show(&status, "the")
            .unwrap_err()
            .contains("several")
    );
    assert!(resolve_show(&status, "missing").is_err());
    assert!(resolve_show(&status, "").is_err());
}

#[test]
fn frames_are_bounded_and_malformed_requests_are_rejected() {
    assert!(read_line(&mut Cursor::new(vec![b'x'; MAX_FRAME as usize + 1])).is_err());
    assert_eq!(
        read_line(&mut Cursor::new(b"line\n")).unwrap(),
        Some("line\n".into())
    );
    assert!(read_line(&mut Cursor::new(b"")).unwrap().is_none());
    assert!(
        serde_json::from_str::<Request>(r#"{"id":1,"command":{"action":"close_window"}}"#).is_err()
    );
}

#[test]
fn live_stream_parses_multiline_unicode_status_and_reports_closure() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_string();
    let worker = std::thread::spawn(move || {
        let req = server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        req.respond(Response::from_string(identity().to_string()))
            .unwrap();
        let req = server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert_eq!(req.url(), "/status/stream");
        let body = ": keepalive\n\nevent: status\ndata: {\"phase\":\"playing\",\ndata: \"message\":\"Télévision\"}\n\n";
        req.respond(
            Response::from_string(body)
                .with_header(Header::from_bytes("Content-Type", "text/event-stream").unwrap()),
        )
        .unwrap();
    });
    let client = Client::connect_to(&address).unwrap();
    let mut received = Vec::new();
    let error = client
        .watch(|frame| {
            received.push(frame);
            Ok(())
        })
        .unwrap_err();
    assert!(matches!(&received[0], Frame::Status { status } if status["message"] == "Télévision"));
    assert!(error.contains("closed"));
    worker.join().unwrap();
}
