//! The libmpv player: wraps an [`mpv::Handle`] with the queue/wait/clear/OSD
//! operations the runner needs (via [`shows_core::runner::PlayerOps`]) and the
//! seek/volume/track controls the control server needs. An event-pump thread
//! turns libmpv's C events into the runner's callbacks.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use shows_core::runner::{PlayerOps, PlayerShutdown, RoundEndTracker, StopFlag};

use crate::mpv::{
    Handle, MPV_END_FILE_REASON_EOF, MPV_EVENT_END_FILE, MPV_EVENT_FILE_LOADED, MPV_EVENT_SHUTDOWN,
    MpvEventEndFile,
};

const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;
const MPV_FORMAT_STRING: c_int = 1;

#[repr(C)]
struct MpvEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

type Cb = Box<dyn Fn() + Send + Sync>;
type PosCb = Box<dyn Fn(usize) + Send + Sync>;

const PLAYBACK_OBSERVED_PROPERTIES: &[&str] = &[
    "time-pos",
    "duration",
    "percent-pos",
    "volume",
    "pause",
    "sid",
    "aid",
    "track-list/count",
];

struct Shared {
    // Round boundary, driven by playlist-pos (-1 == exhausted). The tracker
    // counts a round end once per real exhaustion and ignores the startup/idle
    // -1, so in-round navigation (skip/prev) never miscounts the boundary.
    round: RoundEndTracker,
    shutdown: bool,
    interrupted: bool,
}

pub struct Player {
    handle: Arc<Handle>,
    cv: Condvar,
    shared: Mutex<Shared>,
    on_natural_end: Mutex<Option<Cb>>,
    on_file_loaded: Mutex<Option<Cb>>,
    on_playback_change: Mutex<Option<Cb>>,
    on_pos: Mutex<Option<PosCb>>,
}

impl Player {
    /// Wrap the handle and start the event pump. The handle is shared with the
    /// compositor's render context, so it's an `Arc`.
    pub fn new(handle: Arc<Handle>) -> Arc<Player> {
        let player = Arc::new(Player {
            handle,
            cv: Condvar::new(),
            shared: Mutex::new(Shared {
                round: RoundEndTracker::default(),
                shutdown: false,
                interrupted: false,
            }),
            on_natural_end: Mutex::new(None),
            on_file_loaded: Mutex::new(None),
            on_playback_change: Mutex::new(None),
            on_pos: Mutex::new(None),
        });
        // Report which queued entry is playing so the runner routes skip/defer to
        // it and the overlay shows now-playing.
        player.handle.observe_property_string(1, "playlist-pos");
        for (i, name) in PLAYBACK_OBSERVED_PROPERTIES.iter().enumerate() {
            player.handle.observe_property_string(10 + i as u64, name);
        }
        let pump = player.clone();
        std::thread::Builder::new()
            .name("mpv-events".into())
            .spawn(move || pump.event_loop())
            .expect("spawn mpv event pump");
        player
    }

    pub fn set_on_natural_end(&self, f: Cb) {
        *self.on_natural_end.lock().unwrap() = Some(f);
    }
    pub fn set_on_file_loaded(&self, f: Cb) {
        *self.on_file_loaded.lock().unwrap() = Some(f);
    }
    pub fn set_on_playback_change(&self, f: Cb) {
        *self.on_playback_change.lock().unwrap() = Some(f);
    }
    pub fn set_on_pos(&self, f: PosCb) {
        *self.on_pos.lock().unwrap() = Some(f);
    }

    fn notify_playback_change(&self) {
        if let Some(cb) = self.on_playback_change.lock().unwrap().as_ref() {
            cb();
        }
    }

    fn event_loop(self: Arc<Self>) {
        loop {
            let ev = self.handle.wait_event(-1.0); // block until the next event
            if ev.is_null() {
                continue;
            }
            let event_id = unsafe { (*ev).event_id };
            match event_id {
                MPV_EVENT_SHUTDOWN => {
                    {
                        let mut s = self.shared.lock().unwrap();
                        s.shutdown = true;
                    }
                    self.cv.notify_all();
                    return; // mpv core is going away; stop pumping
                }
                MPV_EVENT_END_FILE => {
                    // Only a natural end (played to completion) marks the episode
                    // watched. The round boundary is detected from playlist-pos
                    // -> -1 (below), not by counting end-file events, so a
                    // replayed entry (playlist-prev) never miscounts the round.
                    let natural = unsafe {
                        let d = (*ev).data as *const MpvEventEndFile;
                        !d.is_null() && (*d).reason == MPV_END_FILE_REASON_EOF
                    };
                    if natural {
                        if let Some(cb) = self.on_natural_end.lock().unwrap().as_ref() {
                            cb();
                        }
                    }
                }
                MPV_EVENT_FILE_LOADED => {
                    if let Some(cb) = self.on_file_loaded.lock().unwrap().as_ref() {
                        cb();
                    }
                    self.notify_playback_change();
                }
                MPV_EVENT_PROPERTY_CHANGE => unsafe {
                    let prop = (*ev).data as *const MpvEventProperty;
                    if prop.is_null() {
                        continue;
                    }
                    let name = if (*prop).name.is_null() {
                        ""
                    } else {
                        CStr::from_ptr((*prop).name).to_str().unwrap_or("")
                    };
                    if name == "playlist-pos"
                        && (*prop).format == MPV_FORMAT_STRING
                        && !(*prop).data.is_null()
                    {
                        let sptr = *((*prop).data as *const *const c_char);
                        if !sptr.is_null() {
                            if let Ok(i) = CStr::from_ptr(sptr).to_string_lossy().parse::<i64>() {
                                // Feed the tracker: a -1 after real playback is
                                // the round boundary that wakes
                                // wait_for_round_end (keep-open=no lets the last
                                // entry advance past its end so this fires
                                // instead of pausing on the final frame). The
                                // startup/idle -1 returns false and is ignored.
                                let ended = self.shared.lock().unwrap().round.observe(i);
                                if ended {
                                    self.cv.notify_all();
                                }
                                if i >= 0 {
                                    if let Some(cb) = self.on_pos.lock().unwrap().as_ref() {
                                        cb(i as usize);
                                    }
                                }
                            }
                        }
                    }
                    if PLAYBACK_OBSERVED_PROPERTIES.contains(&name) {
                        self.notify_playback_change();
                    }
                },
                _ => {}
            }
        }
    }

    // ── control-server playback controls ─────────────────────────────────
    pub fn toggle_pause(&self) {
        self.handle.command(&["cycle", "pause"]);
    }
    pub fn seek_relative(&self, seconds: f64) {
        self.handle
            .command(&["seek", &seconds.to_string(), "relative"]);
    }
    pub fn seek_percent(&self, pct: f64) {
        self.handle.command(&[
            "seek",
            &pct.clamp(0.0, 100.0).to_string(),
            "absolute-percent",
        ]);
    }
    pub fn set_volume(&self, vol: f64) {
        self.handle
            .set_property("volume", &vol.clamp(0.0, 130.0).to_string());
    }
    pub fn set_sub(&self, sid: &str) {
        self.handle.set_property("sid", sid); // an id, or "no" to disable
    }
    pub fn set_audio(&self, aid: &str) {
        self.handle.set_property("aid", aid);
    }

    fn prop_f64(&self, name: &str) -> Option<f64> {
        self.handle
            .get_property(name)
            .and_then(|s| s.parse::<f64>().ok())
    }

    fn tracks(&self, kind: &str) -> Vec<Value> {
        let count: i64 = self
            .handle
            .get_property("track-list/count")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut out = Vec::new();
        for i in 0..count {
            if self
                .handle
                .get_property(&format!("track-list/{i}/type"))
                .as_deref()
                != Some(kind)
            {
                continue;
            }
            let id = self
                .handle
                .get_property(&format!("track-list/{i}/id"))
                .unwrap_or_default();
            let title = self.handle.get_property(&format!("track-list/{i}/title"));
            let lang = self.handle.get_property(&format!("track-list/{i}/lang"));
            let selected = self
                .handle
                .get_property(&format!("track-list/{i}/selected"))
                .map(|s| s == "yes")
                .unwrap_or(false);
            out.push(json!({
                "id": id.parse::<i64>().ok(),
                "title": title.clone().or_else(|| lang.clone()).unwrap_or_else(|| format!("{kind} {id}")),
                "lang": lang,
                "selected": selected,
            }));
        }
        out
    }

    /// Snapshot of current playback for the overlay's scrub bar + track menus.
    pub fn playback_state(&self) -> Value {
        json!({
            "time_pos": self.prop_f64("time-pos"),
            "duration": self.prop_f64("duration"),
            "percent_pos": self.prop_f64("percent-pos"),
            "volume": self.prop_f64("volume"),
            "paused": self.handle.get_property("pause").map(|s| s == "yes").unwrap_or(false),
            "sub_tracks": self.tracks("sub"),
            "audio_tracks": self.tracks("audio"),
            "sid": self.handle.get_property("sid"),
            "aid": self.handle.get_property("aid"),
        })
    }

    /// Retrieve the width and height of the currently playing video.
    pub fn video_dimensions(&self) -> Option<(i32, i32)> {
        let w = self.prop_f64("video-params/w")? as i32;
        let h = self.prop_f64("video-params/h")? as i32;
        if w > 0 && h > 0 { Some((w, h)) } else { None }
    }
}

impl PlayerOps for Player {
    fn play(&self, path: &str, mode: &str) {
        // mode: "replace" for the first entry, "append-play" for the rest.
        self.handle.command(&["loadfile", path, mode]);
    }
    fn playlist_clear(&self) {
        {
            let mut s = self.shared.lock().unwrap();
            s.interrupted = true;
        }
        self.cv.notify_all();
        self.handle.command(&["playlist-clear"]);
    }
    fn show_text(&self, text: &str, duration_ms: i64) {
        self.handle
            .command(&["show-text", text, &duration_ms.to_string()]);
    }
    fn skip(&self) {
        // Force-advance to the next queued entry. At the last entry this
        // exhausts the playlist (playlist-pos -> -1), which ends the round.
        self.handle.command(&["playlist-next", "force"]);
    }
    fn previous(&self) {
        // Step back to the previous queued entry. "weak" so going back at the
        // first entry is a no-op rather than terminating playback.
        self.handle.command(&["playlist-prev", "weak"]);
    }
    fn time_pos(&self) -> Option<f64> {
        self.prop_f64("time-pos")
    }
    fn seek_absolute(&self, seconds: f64) {
        self.handle
            .command(&["seek", &seconds.to_string(), "absolute"]);
    }
    fn set_playlist_pos(&self, idx: usize) {
        self.handle.set_property("playlist-pos", &idx.to_string());
    }
    fn wait_for_round_end(&self, stop: &StopFlag) -> Result<(), PlayerShutdown> {
        let mut s = self.shared.lock().unwrap();
        s.interrupted = false; // Reset interrupted status before waiting
        let target = s.round.ended_count() + 1;
        while s.round.ended_count() < target {
            if s.shutdown {
                return Err(PlayerShutdown("mpv shutdown event".into()));
            }
            if stop.is_set() {
                return Err(PlayerShutdown("runner stopped".into()));
            }
            if s.interrupted {
                return Err(PlayerShutdown("interrupted".into()));
            }
            let (guard, _) = self.cv.wait_timeout(s, Duration::from_millis(500)).unwrap();
            s = guard;
        }
        Ok(())
    }
}
