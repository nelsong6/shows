use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame as TerminalFrame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;
use shows_control::{Command, Frame};

use crate::{
    clock, current, text,
    transport::{REQUEST_TIMEOUT, Session},
};

struct Model {
    status: Value,
    connected: bool,
    selected: usize,
    notice: String,
    pending: Option<(u64, Instant)>,
}

impl Model {
    fn connecting(id: u64) -> Self {
        Self {
            status: Value::Null,
            connected: false,
            selected: 0,
            notice: "Connecting to the running Shows instance…".into(),
            pending: Some((id, Instant::now())),
        }
    }

    fn disconnect(&mut self, message: String) {
        self.connected = false;
        self.pending = None;
        self.notice =
            format!("Disconnected: {message}  Press r to reconnect; commands are never replayed.");
    }

    fn receive(&mut self, frame: Frame) {
        match frame {
            Frame::Status { status } => {
                if self.status.is_null() {
                    self.selected = status["round_pos"].as_u64().unwrap_or(0) as usize;
                    self.notice = "Connected. Controls affect only the target PC.".into();
                }
                self.connected = true;
                self.status = status;
                self.selected = self.selected.min(self.round_len().saturating_sub(1));
            }
            Frame::Reply { id, result } if self.pending.is_some_and(|p| p.0 == id) => {
                self.pending = None;
                self.notice = match result {
                    Ok(result) => result["message"]
                        .as_str()
                        .unwrap_or("Target accepted the request")
                        .to_string(),
                    Err(error) => format!("Request failed: {error}"),
                };
            }
            Frame::Disconnected { message } => self.disconnect(message),
            _ => (),
        }
    }

    fn round_len(&self) -> usize {
        self.status["round"].as_array().map_or(0, Vec::len)
    }

    fn ready(&self) -> bool {
        self.connected && self.pending.is_none() && shows_control::playback_ready(&self.status)
    }

    fn control(&self, key: KeyCode) -> Option<Command> {
        let playback = &self.status["playback"];
        match key {
            KeyCode::Char(' ') => Some(if playback["paused"] == true {
                Command::Play {}
            } else {
                Command::Pause {}
            }),
            KeyCode::Char('s') => Some(Command::Play {}),
            KeyCode::Char('p') => Some(Command::Pause {}),
            KeyCode::Char('n') => Some(Command::Skip {}),
            KeyCode::Char('b') => Some(Command::Previous {}),
            KeyCode::Char('d') => Some(Command::Defer {}),
            KeyCode::Left => Some(Command::Seek { seconds: -10.0 }),
            KeyCode::Right => Some(Command::Seek { seconds: 10.0 }),
            KeyCode::Char('+') | KeyCode::Char('=') => Some(Command::Volume {
                level: (playback["volume"].as_f64()? + 5.0).clamp(0.0, 130.0),
            }),
            KeyCode::Char('-') => Some(Command::Volume {
                level: (playback["volume"].as_f64()? - 5.0).clamp(0.0, 130.0),
            }),
            KeyCode::Enter => Some(Command::PlayShow {
                show_id: self.status["round"][self.selected]["show_id"]
                    .as_str()?
                    .to_string(),
            }),
            _ => None,
        }
    }
}

fn draw(frame: &mut TerminalFrame, model: &Model, target: &str) {
    let areas = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(4),
        Constraint::Length(3),
    ])
    .split(frame.area());
    let playback = &model.status["playback"];
    let state = if !model.connected {
        "DISCONNECTED"
    } else if playback["paused"] == true {
        "PAUSED"
    } else {
        text(&model.status["phase"])
    };
    let color = if model.connected {
        Color::Cyan
    } else {
        Color::Yellow
    };
    let headline = format!(
        "{}  |  {} / {}  |  volume {}\n{}",
        text(&current(&model.status)["show_name"]),
        clock(&playback["time_pos"]),
        clock(&playback["duration"]),
        playback["volume"]
            .as_f64()
            .map_or("—".into(), |v| format!("{v:.0}")),
        text(&model.status["message"])
    );
    frame.render_widget(
        Paragraph::new(headline)
            .block(
                Block::bordered()
                    .title(format!(" Shows Remote → {target}  ·  {state} "))
                    .border_style(Style::default().fg(color)),
            )
            .wrap(Wrap { trim: true }),
        areas[0],
    );
    let percent = playback["percent_pos"]
        .as_f64()
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    frame.render_widget(
        Gauge::default()
            .block(Block::bordered().title(" Playback "))
            .ratio(percent / 100.0)
            .gauge_style(Style::default().fg(color)),
        areas[1],
    );
    let pos = model.status["round_pos"].as_u64().unwrap_or(0) as usize;
    let items: Vec<ListItem> = model.status["round"]
        .as_array()
        .map(|round| {
            round
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    ListItem::new(format!(
                        "{} {:>2}. {}",
                        if i == pos { ">" } else { " " },
                        i + 1,
                        text(&entry["show_name"])
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let list = List::new(items)
        .block(Block::bordered().title(" Target's current round · ↑↓ select · Enter play "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut selection = ListState::default().with_selected(if model.round_len() > 0 {
        Some(model.selected)
    } else {
        None
    });
    frame.render_stateful_widget(list, areas[2], &mut selection);
    let alerts = model.status["alerts"]
        .as_array()
        .map(|alerts| {
            alerts
                .iter()
                .filter_map(|a| a["message"].as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default();
    let notice = format!(
        "{}{}{}",
        if model.pending.is_some() {
            "Waiting for target… "
        } else {
            ""
        },
        model.notice,
        if alerts.is_empty() {
            String::new()
        } else {
            format!("\n{alerts}")
        }
    );
    frame.render_widget(
        Paragraph::new(notice)
            .block(Block::bordered().title(" Connection / result "))
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(color)),
        areas[3],
    );
    frame.render_widget(Paragraph::new("Space play/pause · s play · p pause · n skip (marks watched) · b previous\nd defer · ←→ seek 10s · +/- volume · r reconnect · q quit")
        .wrap(Wrap { trim: true }), areas[4]);
}

pub fn run(mut session: Session, target: &str, channel: &str, local: bool) -> Result<(), String> {
    let id = session.send(Command::Watch {})?;
    let mut model = Model::connecting(id);
    let mut connection = Some(session);
    let mut started = Instant::now();
    let mut last_activity = Instant::now();
    let mut terminal = ratatui::try_init().map_err(|e| e.to_string())?;
    let result = (|| {
        loop {
            let mut lost = false;
            if let Some(session) = connection.as_ref() {
                while let Ok(frame) = session.events.try_recv() {
                    last_activity = Instant::now();
                    lost |= matches!(&frame, Frame::Disconnected { .. });
                    model.receive(frame);
                }
            }
            if lost {
                connection.take();
            }
            if model.connected && last_activity.elapsed() > Duration::from_secs(65) {
                model.disconnect("No status or heartbeat from Shows for 65 seconds".into());
                connection.take();
            }
            if model
                .pending
                .is_some_and(|p| p.1.elapsed() > REQUEST_TIMEOUT)
                || (!model.connected && connection.is_some() && started.elapsed() > REQUEST_TIMEOUT)
            {
                model.disconnect("Response timed out. A sent command's outcome may be unknown; inspect status before retrying.".into());
                connection.take();
            }
            terminal
                .draw(|frame| draw(frame, &model, if local { "this PC" } else { target }))
                .map_err(|e| e.to_string())?;
            if !event::poll(Duration::from_millis(100)).map_err(|e| e.to_string())? {
                continue;
            }
            let Event::Key(key) = event::read().map_err(|e| e.to_string())? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return Ok(());
            }
            match key.code {
                KeyCode::Down => {
                    model.selected = (model.selected + 1).min(model.round_len().saturating_sub(1))
                }
                KeyCode::Up => model.selected = model.selected.saturating_sub(1),
                KeyCode::Char('r') if !model.connected => {
                    connection.take();
                    match Session::connect(target, channel, local)
                        .and_then(|mut s| s.send(Command::Watch {}).map(|id| (s, id)))
                    {
                        Ok((session, id)) => {
                            connection = Some(session);
                            model = Model::connecting(id);
                            started = Instant::now();
                            last_activity = Instant::now();
                        }
                        Err(message) => model.disconnect(message),
                    }
                }
                _ if model.ready() => {
                    if let Some(command) = model.control(key.code) {
                        match connection
                            .as_mut()
                            .ok_or("connection unavailable")
                            .and_then(|s| {
                                s.send(command)
                                    .map_err(|_| "could not send control; reconnect")
                            }) {
                            Ok(id) => model.pending = Some((id, Instant::now())),
                            Err(error) => model.disconnect(error.into()),
                        }
                    }
                }
                _ => (),
            }
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    #[test]
    fn disconnected_and_waiting_states_disable_controls_without_optimistic_playback() {
        let mut model = Model::connecting(1);
        assert!(!model.ready());
        model.receive(Frame::Reply {
            id: 1,
            result: Ok(json!({})),
        });
        model.receive(Frame::Status {
            status: json!({"phase":"playing", "round_pos":0, "round":[{"episode_id":"e1"}], "playback":{"paused":true,"time_pos":0,"core_idle":true}}),
        });
        assert!(model.ready());
        assert_eq!(model.control(KeyCode::Char(' ')), Some(Command::Play {}));
        model.pending = Some((2, Instant::now()));
        assert!(!model.ready());
        assert_eq!(model.status["playback"]["paused"], true);
        model.disconnect("network lost".into());
        assert!(!model.ready());
    }

    #[test]
    fn terminal_renders_target_failures_and_survives_small_resizes() {
        let mut model = Model::connecting(1);
        model.disconnect("SSH refused".into());
        for (width, height) in [(100, 24), (60, 15), (20, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| draw(f, &model, "upstairspc")).unwrap();
            if width == 100 {
                let rendered = format!("{:?}", terminal.backend().buffer());
                assert!(rendered.contains("upstairspc"));
                assert!(rendered.contains("SSH refused"));
            }
        }
    }
}
