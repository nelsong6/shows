use crate::{
    current,
    picker::{self, Choice},
    text,
    transport::{Session, valid_host},
};
use serde_json::Value;
use shows_control::Command;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub host: String,
    pub local: bool,
}

fn unavailable(error: &str) -> &str {
    if error.contains("Shows is not available") {
        "Open Shows on this PC"
    } else if error.contains("Permission denied") || error.contains("Host key verification failed")
    {
        "Existing SSH access needs setup"
    } else if error.contains("cannot find the path") || error.contains("not recognized") {
        "Install the current Shows release to enable remote control"
    } else if error.contains("timed out")
        || error.contains("Could not resolve hostname")
        || error.contains("No route")
        || error.contains("refused")
    {
        "PC is unreachable or SSH is not running"
    } else if error.contains("compatible Shows") {
        "Update Shows and the remote together"
    } else {
        error
    }
}

pub fn targets(inventory: &Value, computer: &str) -> Result<Vec<Target>, String> {
    let machines = inventory["machines"]
        .as_object()
        .ok_or("Home inventory has no machines")?;
    if machines.len() > 64 {
        return Err("Home inventory exceeds 64 machines".into());
    }
    let mut result = Vec::new();
    for (host, machine) in machines {
        if !valid_host(host) {
            return Err(format!("Invalid computer name in home inventory: {host}"));
        }
        result.push(Target {
            host: host.clone(),
            local: machine["computer"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(computer)),
        });
    }
    Ok(result)
}

fn inventory() -> Result<Vec<Target>, String> {
    let path = std::env::var_os("SHOWS_REMOTE_INVENTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(r"\\NelsonNAS\files\shell-config-profile-home\machine\state.json")
        });
    let data = std::fs::read(&path).map_err(|e| {
        format!(
            "Cannot read home computer inventory {}: {e}",
            path.display()
        )
    })?;
    if data.len() > 1024 * 1024 {
        return Err("Home inventory exceeds 1 MiB".into());
    }
    let value = serde_json::from_slice(&data)
        .map_err(|e| format!("Invalid home computer inventory: {e}"))?;
    targets(&value, &std::env::var("COMPUTERNAME").unwrap_or_default())
}

pub fn choose(channel: &str) -> Result<Option<Target>, String> {
    picker::executable()?;
    loop {
        println!("Looking for running Shows instances…");
        let candidates = inventory()?;
        let (tx, rx) = std::sync::mpsc::channel();
        for target in candidates.iter().cloned() {
            let tx = tx.clone();
            let channel = channel.to_owned();
            std::thread::spawn(move || {
                let result = Session::connect_native(&target.host, &channel, target.local)
                    .and_then(|mut session| session.call(Command::Status {}));
                let _ = tx.send((target, result));
            });
        }
        drop(tx);
        let mut results: Vec<_> = rx.into_iter().collect();
        results.sort_by_key(|(target, result)| (result.is_err(), target.host.clone()));
        let mut choices = Vec::new();
        for (index, (target, result)) in results.iter().enumerate() {
            let description = match result {
                Ok(status) => format!(
                    "{} · {} · {}",
                    if target.local { "this PC" } else { "available" },
                    text(&current(status)["show_name"]),
                    if status["playback"]["paused"] == true {
                        "paused"
                    } else {
                        text(&status["phase"])
                    }
                ),
                Err(error) => format!("Unavailable: {}", unavailable(error)),
            };
            choices.push(Choice {
                id: index.to_string(),
                name: target.host.clone(),
                description,
            });
        }
        choices.push(Choice {
            id: "refresh".into(),
            name: "Refresh instances".into(),
            description: "Check again after opening Shows or bringing a PC online".into(),
        });
        choices.push(Choice {
            id: "quit".into(),
            name: "Quit".into(),
            description: "Leave all players running".into(),
        });
        let title = if results.iter().any(|(_, result)| result.is_ok()) {
            "Shows remote — choose an instance"
        } else {
            "No running Shows instances — open Shows, then refresh"
        };
        loop {
            match picker::select(title, &choices)?.as_deref() {
                None | Some("quit") => return Ok(None),
                Some("refresh") => break,
                Some(index) => {
                    let (target, status) =
                        &results[index.parse::<usize>().map_err(|e| e.to_string())?];
                    if status.is_ok() {
                        return Ok(Some(target.clone()));
                    }
                    // Keep unavailable PCs visible, without trying to control them.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn inventory_includes_every_pc_and_identifies_self_without_ssh() {
        let found = targets(
            &json!({"machines":{"livingroom":{"computer":"TV"},"desk":{"computer":"DESK"}}}),
            "desk",
        )
        .unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|t| t.host == "desk" && t.local));
        assert!(found.iter().any(|t| t.host == "livingroom" && !t.local));
        assert!(targets(&json!({"machines":{"-oProxyCommand=bad":{}}}), "desk").is_err());
    }
}
