use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame as TerminalFrame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;
use shows_control::preview::{INTERVAL, VideoPreview};
use shows_control::{Command, Frame};

use crate::{
    clock, current,
    picker::{self, Choice},
    text,
    transport::{REQUEST_TIMEOUT, Session},
};

struct Model {
    status: Value,
    connected: bool,
    selected: usize,
    notice: String,
    pending: Option<(u64, Instant)>,
    visualizer: bool,
    preview: Option<(VideoPreview, Instant)>,
    preview_pending: Option<(u64, Instant)>,
    preview_attempt: Option<Instant>,
    preview_error: Option<String>,
}

impl Model {
    fn connecting(id: u64) -> Self {
        Self {
            status: Value::Null,
            connected: false,
            selected: 0,
            notice: "Connecting to the running Shows instance…".into(),
            pending: Some((id, Instant::now())),
            visualizer: false,
            preview: None,
            preview_pending: None,
            preview_attempt: None,
            preview_error: None,
        }
    }

    fn disconnect(&mut self, message: String) {
        self.connected = false;
        self.pending = None;
        self.preview = None;
        self.preview_pending = None;
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
            Frame::Reply { id, result } if self.preview_pending.is_some_and(|p| p.0 == id) => {
                self.preview_pending = None;
                if self.visualizer {
                    match result.and_then(|value| {
                        let frame: VideoPreview = serde_json::from_value(value)
                            .map_err(|e| format!("Invalid video preview: {e}"))?;
                        frame.validate()?;
                        Ok(frame)
                    }) {
                        Ok(frame) => {
                            self.preview = Some((frame, Instant::now()));
                            self.preview_error = None;
                        }
                        Err(error) => {
                            self.preview = None;
                            self.preview_error = Some(error);
                        }
                    }
                }
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

    fn toggle_visualizer(&mut self) {
        self.visualizer = !self.visualizer;
        self.preview = None;
        self.preview_error = None;
        self.preview_attempt = None;
    }

    fn request_preview(&mut self, session: &mut Session) {
        if self
            .preview_pending
            .is_some_and(|p| p.1.elapsed() > Duration::from_secs(2))
        {
            // Keep the outstanding id until its reply arrives; never queue another read behind it.
            self.preview = None;
            self.preview_error =
                Some("Video preview timed out; playback controls remain available".into());
        }
        let interval = if self.preview_error.is_some() {
            Duration::from_secs(2)
        } else {
            INTERVAL
        };
        if self.visualizer
            && self.connected
            && self.pending.is_none()
            && self.preview_pending.is_none()
            && self
                .preview_attempt
                .is_none_or(|at| at.elapsed() >= interval)
        {
            self.preview_attempt = Some(Instant::now());
            match session.send(Command::Preview {}) {
                Ok(id) => self.preview_pending = Some((id, Instant::now())),
                Err(error) => self.disconnect(error),
            }
        }
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
            KeyCode::Char('f') => Some(Command::Fullscreen {}),
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
                    .title(format!(
                        " Shows Remote → {target}  ·  {state}{} ",
                        if model.status["window_fullscreen"] == true {
                            " · fullscreen"
                        } else {
                            ""
                        }
                    ))
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
    if model.visualizer {
        let block = Block::bordered().title(" ASCII video · v playlist ");
        let mut area = block.inner(areas[2]);
        frame.render_widget(block, areas[2]);
        let lines = if !model.connected {
            vec!["Disconnected — press r to reconnect".into()]
        } else if let Some(error) = model.preview_error.as_ref() {
            vec![format!("Video preview unavailable: {error}")]
        } else if let Some((preview, at)) = model.preview.as_ref() {
            if at.elapsed() > Duration::from_secs(2) {
                vec!["Waiting for a fresh video frame…".into()]
            } else {
                preview.fit(area.width, area.height)
            }
        } else {
            vec!["Loading video preview…".into()]
        };
        area.y += area.height.saturating_sub(lines.len() as u16) / 2;
        area.height = area.height.min(lines.len() as u16);
        frame.render_widget(
            Paragraph::new(lines.join("\n"))
                .alignment(ratatui::layout::Alignment::Center)
                .style(Style::default().fg(Color::White).bg(Color::Black)),
            area,
        );
    } else {
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
    }
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
    frame.render_widget(Paragraph::new("/ commands · v video/playlist · f fullscreen · Space play/pause\nn skip (marks watched) · b previous · d defer · ←→ seek · +/- volume · r reconnect · q quit")
        .wrap(Wrap { trim: true }), areas[4]);
}

fn palette(model: &Model) -> Result<Option<String>, String> {
    let mut choices = vec![
        Choice {
            id: "visualizer".into(),
            name: if model.visualizer {
                "Show playlist"
            } else {
                "Show ASCII video"
            }
            .into(),
            description: "Switch the remote's view; leave TV playback unchanged".into(),
        },
        Choice {
            id: "switch".into(),
            name: "Change instance".into(),
            description: "Choose another running Shows player".into(),
        },
        Choice {
            id: "reconnect".into(),
            name: "Reconnect".into(),
            description: "Connect again without replaying commands".into(),
        },
        Choice {
            id: "quit".into(),
            name: "Quit remote".into(),
            description: "Leave TV playback running".into(),
        },
    ];
    if model.connected && model.pending.is_none() {
        choices.push(Choice {
            id: "fullscreen".into(),
            name: "Toggle fullscreen".into(),
            description: "Change the controlled TV window; leave playback unchanged".into(),
        });
    }
    if model.ready() {
        for (id, name) in [
            ("play", "Play"),
            ("pause", "Pause"),
            ("skip", "Next (mark watched)"),
            ("previous", "Previous"),
            ("defer", "Defer (unwatched)"),
            ("back", "Seek back 10 seconds"),
            ("forward", "Seek forward 10 seconds"),
            ("quieter", "Volume down 5"),
            ("louder", "Volume up 5"),
        ] {
            choices.push(Choice {
                id: id.into(),
                name: name.into(),
                description: "Control the selected instance".into(),
            });
        }
        for (index, episode) in model.status["round"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            choices.push(Choice {
                id: format!("show:{index}"),
                name: format!("Play {}", text(&episode["show_name"])),
                description: "Choose from this instance's current round".into(),
            });
        }
    }
    picker::select("Shows remote — commands", &choices)
}

fn palette_command(model: &Model, action: &str) -> Option<Command> {
    let key = match action {
        "play" => KeyCode::Char('s'),
        "pause" => KeyCode::Char('p'),
        "fullscreen" => KeyCode::Char('f'),
        "skip" => KeyCode::Char('n'),
        "previous" => KeyCode::Char('b'),
        "defer" => KeyCode::Char('d'),
        "back" => KeyCode::Left,
        "forward" => KeyCode::Right,
        "quieter" => KeyCode::Char('-'),
        "louder" => KeyCode::Char('+'),
        _ => {
            let index = action.strip_prefix("show:")?.parse::<usize>().ok()?;
            return Some(Command::PlayShow {
                show_id: model.status["round"][index]["show_id"].as_str()?.to_owned(),
            });
        }
    };
    model.control(key)
}

/// True returns to instance selection; false quits the remote.
pub fn run(mut session: Session, target: &str, channel: &str, local: bool) -> Result<bool, String> {
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
            if let Some(session) = connection.as_mut() {
                model.request_preview(session);
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
                return Ok(false);
            }
            let mut code = key.code;
            let mut selected_command = None;
            if code == KeyCode::Char('/') {
                ratatui::restore();
                let selected = palette(&model);
                terminal = ratatui::try_init().map_err(|e| e.to_string())?;
                terminal.clear().map_err(|e| e.to_string())?;
                // A player may disconnect or change while fzt owns the terminal.
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
                match selected {
                    Ok(Some(action)) if action == "switch" => return Ok(true),
                    Ok(Some(action)) if action == "quit" => return Ok(false),
                    Ok(Some(action)) if action == "visualizer" => {
                        model.toggle_visualizer();
                        continue;
                    }
                    Ok(Some(action)) if action == "reconnect" => {
                        model.disconnect("Reconnecting".into());
                        code = KeyCode::Char('r');
                    }
                    Ok(Some(action)) => selected_command = palette_command(&model, &action),
                    Ok(None) => continue,
                    Err(message) => {
                        model.notice = message;
                        continue;
                    }
                }
            }
            match code {
                KeyCode::Char('v') => model.toggle_visualizer(),
                KeyCode::Enter if model.visualizer && selected_command.is_none() => (),
                KeyCode::Down if !model.visualizer => {
                    model.selected = (model.selected + 1).min(model.round_len().saturating_sub(1))
                }
                KeyCode::Up if !model.visualizer => {
                    model.selected = model.selected.saturating_sub(1)
                }
                KeyCode::Char('r') if !model.connected => {
                    connection.take();
                    match Session::connect(target, channel, local)
                        .and_then(|mut s| s.send(Command::Watch {}).map(|id| (s, id)))
                    {
                        Ok((session, id)) => {
                            connection = Some(session);
                            let visualizer = model.visualizer;
                            model = Model::connecting(id);
                            model.visualizer = visualizer;
                            started = Instant::now();
                            last_activity = Instant::now();
                        }
                        Err(message) => model.disconnect(message),
                    }
                }
                _ if model.ready()
                    || (model.connected
                        && model.pending.is_none()
                        && (code == KeyCode::Char('f')
                            || selected_command == Some(Command::Fullscreen {}))) =>
                {
                    if let Some(command) = selected_command.or_else(|| model.control(code)) {
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
    fn preview_replies_do_not_acknowledge_controls_and_disabling_discards_late_frames() {
        let mut model = Model::connecting(1);
        model.connected = true;
        model.toggle_visualizer();
        model.preview_pending = Some((2, Instant::now()));
        model.receive(Frame::Reply {
            id: 2,
            result: Err("window is covered".into()),
        });
        assert_eq!(model.pending.unwrap().0, 1);
        assert_eq!(model.preview_error.as_deref(), Some("window is covered"));
        model.preview_pending = Some((3, Instant::now()));
        model.toggle_visualizer();
        let preview = VideoPreview::from_rgba(&vec![0; 160 * 90 * 4], 1.0, 0.1).unwrap();
        model.receive(Frame::Reply {
            id: 3,
            result: Ok(serde_json::to_value(preview).unwrap()),
        });
        assert!(model.preview.is_none());
        assert!(model.preview_pending.is_none());
    }

    #[test]
    fn visualizer_replaces_round_and_shows_errors_at_small_sizes() {
        let mut model = Model::connecting(1);
        model.connected = true;
        model.status = json!({"round_pos":0,"round":[{"show_name":"Current show"},{"show_name":"Hidden round entry"}]});
        model.toggle_visualizer();
        model.preview_error = Some("window is covered".into());
        for (width, height) in [(100, 30), (20, 8), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| draw(f, &model, "upstairspc")).unwrap();
            if width == 100 {
                let rendered = format!("{:?}", terminal.backend().buffer());
                assert!(rendered.contains("window is covered"));
                assert!(!rendered.contains("Hidden round entry"));
                assert!(rendered.contains("video/playlist"));
            }
        }
    }

    #[test]
    fn palette_actions_use_target_show_ids_and_existing_explicit_commands() {
        let mut model = Model::connecting(1);
        model.status = json!({"round":[{"show_id":"stable-show-id"}],"playback":{"volume":80}});
        assert_eq!(
            palette_command(&model, "show:0"),
            Some(Command::PlayShow {
                show_id: "stable-show-id".into()
            })
        );
        assert_eq!(palette_command(&model, "pause"), Some(Command::Pause {}));
        assert_eq!(
            palette_command(&model, "fullscreen"),
            Some(Command::Fullscreen {})
        );
        assert_eq!(
            palette_command(&model, "back"),
            Some(Command::Seek { seconds: -10.0 })
        );
        assert_eq!(palette_command(&model, "switch"), None);
        assert_eq!(palette_command(&model, "quit"), None);
        assert_eq!(palette_command(&model, "show:100"), None);
    }

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
