//! fzt owns its terminal while a picker is open; no shell interprets its output.
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub struct Choice {
    pub id: String,
    pub name: String,
    pub description: String,
}

fn field(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

pub fn rows(choices: &[Choice]) -> String {
    choices
        .iter()
        .map(|c| {
            format!(
                "{}\t{}\t{}\n",
                field(&c.name),
                field(&c.description),
                field(&c.id)
            )
        })
        .collect()
}

pub fn executable() -> Result<PathBuf, String> {
    let root =
        std::env::var_os("ProgramFiles").ok_or("The managed fzt installation requires Windows")?;
    let path = PathBuf::from(root).join("at-home/fzt/current/fzt.exe");
    if !path.is_file() {
        return Err(
            "fzt is not installed. Run home-sync sync to install the home terminal tools.".into(),
        );
    }
    Ok(path)
}

pub fn select(title: &str, choices: &[Choice]) -> Result<Option<String>, String> {
    let mut child = Command::new(executable()?)
        .args([
            "--border",
            "--title",
            title,
            "--header",
            "Action\tDetails",
            "--nth",
            "1,2",
            "--accept-nth",
            "3",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("Could not open fzt: {e}"))?;
    let written = child
        .stdin
        .take()
        .unwrap()
        .write_all(rows(choices).as_bytes());
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.code() == Some(130) {
        return Ok(None);
    }
    written.map_err(|e| format!("Could not send choices to fzt: {e}"))?;
    if !output.status.success() {
        return Err(format!("fzt exited with {}", output.status));
    }
    let id = String::from_utf8(output.stdout)
        .map_err(|e| e.to_string())?
        .trim()
        .to_owned();
    if !choices.iter().any(|c| c.id == id) {
        return Err("fzt returned an unknown selection".into());
    }
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_text_cannot_inject_picker_rows_or_actions() {
        let rows = rows(&[Choice {
            id: "play".into(),
            name: "a\nquit".into(),
            description: "x\ty\u{1b}[31m".into(),
        }]);
        assert_eq!(rows, "a quit\tx y [31m\tplay\n");
    }
}
