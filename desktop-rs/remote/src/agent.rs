use std::io::{BufReader, Write};
use std::sync::{Arc, Mutex};

use serde_json::json;
use shows_control::{Client, Command, Frame, Request, read_line};

fn emit(output: &Mutex<std::io::Stdout>, frame: Frame) -> Result<(), String> {
    let mut output = output.lock().unwrap();
    serde_json::to_writer(&mut *output, &frame).map_err(|e| e.to_string())?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|e| e.to_string())
}

pub fn run() -> Result<(), String> {
    let client = Client::connect()?;
    let output = Arc::new(Mutex::new(std::io::stdout()));
    let mut input = BufReader::new(std::io::stdin());
    let mut watching = false;
    while let Some(line) = read_line(&mut input)? {
        let request: Request =
            serde_json::from_str(&line).map_err(|e| format!("invalid request: {e}"))?;
        let result = if request.command == (Command::Watch {}) {
            if watching {
                Err("already watching".into())
            } else {
                watching = true;
                let (client, output) = (client.clone(), output.clone());
                std::thread::spawn(move || {
                    let result = client.watch(|frame| emit(&output, frame));
                    if let Err(message) = result {
                        let _ = emit(&output, Frame::Disconnected { message });
                    }
                });
                Ok(json!({"message": "subscribed to the target's live status"}))
            }
        } else {
            client.execute(&request.command)
        };
        emit(
            &output,
            Frame::Reply {
                id: request.id,
                result,
            },
        )?;
    }
    Ok(())
}
