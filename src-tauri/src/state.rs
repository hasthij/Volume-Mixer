use crate::audio::AudioBackend;
use crate::config::{ConfigStore, BANK_COUNT, CHANNEL_COUNT};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter};

pub const STATE_EVENT: &str = "overlay-state";
pub const APPEARANCE_EVENT: &str = "appearance-changed";

#[derive(Clone, Debug, Serialize)]
pub struct ChannelPayload {
    pub app_id: Option<String>,
    pub app_name: Option<String>,
    pub volume: u8,
    /// Mute/solo were removed; these stay (always false) so the overlay's
    /// payload shape doesn't change.
    pub muted: bool,
    pub solo: bool,
    pub bg_color: Option<String>,
    pub sel_color: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OverlayPayload {
    pub connected: bool,
    pub expanded: bool,
    pub bank: usize,
    pub channel: usize,
    pub banks: Vec<Vec<ChannelPayload>>,
    /// Always null: the button hold was removed.
    pub hold: Option<()>,
}

#[derive(Debug)]
pub struct RuntimeState {
    pub expanded: bool,
    pub channel: usize,
    /// Last known live volume per channel (0-100).
    pub volumes: [u8; CHANNEL_COUNT],
}

impl RuntimeState {
    /// Always starts collapsed — the HUD shouldn't pop up expanded (and
    /// non-click-through) the moment the app launches.
    fn from_config(cfg: &crate::config::AppConfig) -> Self {
        Self {
            expanded: false,
            channel: cfg.last_channel.min(CHANNEL_COUNT - 1),
            volumes: Default::default(),
        }
    }
}

pub struct AppState {
    pub config: ConfigStore,
    pub audio: Box<dyn AudioBackend>,
    pub runtime: Mutex<RuntimeState>,
    pub connected: AtomicBool,
    /// True while the frontend has a mouse-only panel open (the app
    /// dropdown or the per-channel color settings). While true, hardware
    /// nav/button input is still read (to keep its own debounce timing
    /// consistent) but not applied — otherwise a joystick nudge (or just
    /// drift) mid-drag yanks the bank/channel out from under an open panel
    /// and closes it on you.
    pub ui_busy: AtomicBool,
    /// JSON of the last state sent to the windows, to skip identical re-sends.
    last_emitted: Mutex<String>,
}

impl AppState {
    pub fn new(config: ConfigStore, audio: Box<dyn AudioBackend>) -> Self {
        let runtime = RuntimeState::from_config(&config.get());
        // Mute/solo were removed: undo any mute an older version saved,
        // or those apps would stay silent with no way to unmute them.
        let cfg = config.get();
        for (b, bank) in cfg.banks.iter().enumerate() {
            for (c, chan) in bank.iter().enumerate() {
                if cfg.muted[b][c] {
                    if let Some(app_id) = &chan.app_id {
                        audio.set_mute(app_id, false);
                    }
                }
            }
        }
        if cfg.muted.iter().flatten().any(|m| *m) {
            config.update(|c| c.muted = Default::default());
        }
        Self {
            config,
            audio,
            runtime: Mutex::new(runtime),
            connected: AtomicBool::new(false),
            ui_busy: AtomicBool::new(false),
            last_emitted: Mutex::new(String::new()),
        }
    }

    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn build_payload(&self) -> OverlayPayload {
        let cfg = self.config.get();
        let runtime = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        let mut banks = Vec::with_capacity(BANK_COUNT);
        for b in 0..BANK_COUNT {
            let mut chans = Vec::with_capacity(CHANNEL_COUNT);
            for c in 0..CHANNEL_COUNT {
                let cc = &cfg.banks[b][c];
                chans.push(ChannelPayload {
                    app_id: cc.app_id.clone(),
                    app_name: cc.app_name.clone(),
                    volume: if b == 0 && cc.app_id.is_some() {
                        runtime.volumes[c]
                    } else {
                        0
                    },
                    muted: false,
                    solo: false,
                    bg_color: cc.bg_color.clone(),
                    sel_color: cc.sel_color.clone(),
                });
            }
            banks.push(chans);
        }
        OverlayPayload {
            connected: self.is_connected(),
            expanded: runtime.expanded,
            bank: 0,
            channel: runtime.channel,
            banks,
            hold: None,
        }
    }

    /// Sends the state to both windows, but only when it differs from the
    /// last one sent. The serial thread calls this on every 20 Hz controller
    /// frame; most of those change nothing, and skipping them saves both
    /// windows a JSON parse and a render pass each time. A window that loads
    /// later asks for the current state with `get_state`, so nothing is lost.
    pub fn emit(&self, app: &AppHandle) {
        let payload = self.build_payload();
        let Ok(json) = serde_json::to_string(&payload) else { return };
        {
            let mut last = self.last_emitted.lock().unwrap_or_else(|e| e.into_inner());
            if *last == json {
                return;
            }
            *last = json;
        }
        let _ = app.emit(STATE_EVENT, payload);
    }
}
