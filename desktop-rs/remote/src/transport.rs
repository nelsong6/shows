use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, Command as ProcessCommand, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::Value;
use shows_control::{Command, Frame, Request, read_line};

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.as_bytes()[0].is_ascii_alphanumeric()
        && host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
}

pub fn remote_command(channel: &str) -> String {
    assert!(matches!(channel, "stable" | "dev"));
    // The verified installer writes this one-line native entry point. Neither
    // JSON commands nor show names enter the remote command line.
    format!(r#""%LOCALAPPDATA%\shows\agents\{channel}.cmd""#)
}

pub struct Session {
    child: Child,
    input: ChildStdin,
    pub events: Receiver<Frame>,
    next_id: u64,
}

impl Session {
    pub fn connect(host: &str, channel: &str, local: bool) -> Result<Self, String> {
        Self::connect_inner(host, channel, local, true)
    }

    pub fn connect_native(host: &str, channel: &str, local: bool) -> Result<Self, String> {
        Self::connect_inner(host, channel, local, false)
    }

    fn connect_inner(
        host: &str,
        channel: &str,
        local: bool,
        allow_development: bool,
    ) -> Result<Self, String> {
        if !valid_host(host) {
            return Err("target must be a computer hostname or SSH host alias".into());
        }
        let development_agent = allow_development
            && cfg!(debug_assertions)
            && std::env::var("SHOWS_REMOTE_DEV_AGENT").as_deref() == Ok("1");
        let mut command = if local || development_agent {
            let mut cmd = ProcessCommand::new(std::env::current_exe().map_err(|e| e.to_string())?);
            cmd.arg("--agent");
            cmd
        } else {
            let mut cmd = ProcessCommand::new("ssh.exe");
            cmd.args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=2",
                host,
                &remote_command(channel),
            ]);
            cmd
        };
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                format!(
                    "could not start {}: {e}",
                    if local {
                        "local agent"
                    } else {
                        "Windows OpenSSH"
                    }
                )
            })?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let errors = child.stderr.take().unwrap();
        let (tx, events) = mpsc::sync_channel(32);
        let diagnostics = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let capture = diagnostics.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(errors);
            while let Ok(Some(line)) = read_line(&mut reader) {
                let mut text = capture.lock().unwrap();
                if text.len() < 8192 {
                    text.push_str(&line);
                }
            }
        });
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            let result = loop {
                match read_line(&mut reader) {
                    Ok(Some(line)) => match serde_json::from_str(&line) {
                        Ok(frame) => {
                            if tx.send(frame).is_err() {
                                return;
                            }
                        }
                        Err(e) => break format!("invalid remote protocol message: {e}"),
                    },
                    Ok(None) => break "remote connection closed".to_string(),
                    Err(e) => break e,
                }
            };
            // stderr may finish just after stdout; give its reader a bounded chance.
            std::thread::sleep(Duration::from_millis(50));
            let details = diagnostics.lock().unwrap().trim().to_string();
            let message = if details.is_empty() {
                result
            } else {
                format!("{result}: {details}")
            };
            let _ = tx.send(Frame::Disconnected { message });
        });
        Ok(Self {
            child,
            input,
            events,
            next_id: 0,
        })
    }

    pub fn send(&mut self, command: Command) -> Result<u64, String> {
        command.validate()?;
        self.next_id += 1;
        let request = Request {
            id: self.next_id,
            command,
        };
        serde_json::to_writer(&mut self.input, &request).map_err(|e| e.to_string())?;
        self.input
            .write_all(b"\n")
            .and_then(|_| self.input.flush())
            .map_err(|e| e.to_string())?;
        Ok(request.id)
    }

    pub fn call(&mut self, command: Command) -> Result<Value, String> {
        let id = self.send(command)?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let event = self.events.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| "Remote response timed out. The outcome may be unknown; check status before retrying.".to_string())?;
            match event {
                Frame::Reply {
                    id: reply_id,
                    result,
                } if id == reply_id => return result,
                Frame::Disconnected { message } => return Err(message),
                _ => (),
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
