use crate::config::CHANNEL_COUNT;
use crate::state::AppState;
use crate::window;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::AppHandle;

const REPEAT_MS: u64 = 340;
const BAUD_RATE: u32 = 115_200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum XAxis {
    Right,
    Left,
    Neutral,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum YAxis {
    Up,
    Down,
    Neutral,
}

#[derive(Clone, Copy, Debug)]
struct HwFrame {
    k: [u8; 4],
    x: XAxis,
    y: YAxis,
    /// Still parsed so the 7-field line format stays valid; the button does nothing.
    #[allow(dead_code)]
    btn: bool,
}

/// Drop any line that doesn't cleanly parse into exactly 7 fields — a torn
/// USB read shouldn't crash the reader thread.
fn parse_line(line: &str) -> Option<HwFrame> {
    let parts: Vec<&str> = line.trim().split(',').collect();
    if parts.len() != 7 {
        return None;
    }
    let mut k = [0u8; 4];
    for i in 0..4 {
        let v: u8 = parts[i].trim().parse().ok()?;
        if v > 100 {
            return None;
        }
        k[i] = v;
    }
    let x = match parts[4].trim() {
        "RIGHT" => XAxis::Right,
        "LEFT" => XAxis::Left,
        "NEUTRAL" => XAxis::Neutral,
        _ => return None,
    };
    let y = match parts[5].trim() {
        "UP" => YAxis::Up,
        "DOWN" => YAxis::Down,
        "NEUTRAL" => YAxis::Neutral,
        _ => return None,
    };
    let btn = match parts[6].trim() {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    Some(HwFrame { k, x, y, btn })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    PrevChannel,
    NextChannel,
}

struct NavState {
    last_dir: Option<Dir>,
    last_fire_at: Instant,
}

impl NavState {
    fn new() -> Self {
        Self {
            last_dir: None,
            last_fire_at: Instant::now(),
        }
    }
}

/// Left/right only: fires when the direction changes, then repeats every
/// REPEAT_MS while held.
fn step_nav(nav: &mut NavState, frame: &HwFrame, now: Instant) -> Option<Dir> {
    let dir = match frame.x {
        XAxis::Right => Some(Dir::NextChannel),
        XAxis::Left => Some(Dir::PrevChannel),
        XAxis::Neutral => None,
    };
    if dir != nav.last_dir {
        nav.last_dir = dir;
        nav.last_fire_at = now;
        return dir;
    }
    if let Some(d) = dir {
        if now.duration_since(nav.last_fire_at) >= Duration::from_millis(REPEAT_MS) {
            nav.last_fire_at = now;
            return Some(d);
        }
    }
    None
}

/// Pulling the stick down toggles the overlay: fires once on the way down,
/// then not again until the stick leaves down.
fn step_pull(was_down: &mut bool, frame: &HwFrame) -> bool {
    let down = frame.y == YAxis::Down;
    let fired = down && !*was_down;
    *was_down = down;
    fired
}

fn process_frame(app: &AppHandle, state: &Arc<AppState>, nav: &mut NavState, was_down: &mut bool, frame: HwFrame) {
    let now = Instant::now();
    let cfg = state.config.get();
    let mut runtime = state.runtime.lock().unwrap_or_else(|e| e.into_inner());

    for c in 0..CHANNEL_COUNT {
        runtime.volumes[c] = frame.k[c];
        if let Some(app_id) = &cfg.banks[0][c].app_id {
            state.audio.set_volume(app_id, frame.k[c] as f32 / 100.0);
        }
    }

    let dir = step_nav(nav, &frame, now);
    let prev_channel = runtime.channel;

    let pulled = step_pull(was_down, &frame);
    if !state.ui_busy.load(Ordering::Relaxed) {
        if pulled {
            runtime.expanded = !runtime.expanded;
            window::set_overlay_expanded(app, runtime.expanded);
        } else if runtime.expanded {
            match dir {
                Some(Dir::PrevChannel) => runtime.channel = runtime.channel.saturating_sub(1),
                Some(Dir::NextChannel) => runtime.channel = (runtime.channel + 1).min(CHANNEL_COUNT - 1),
                None => {}
            }
        }
    }

    if runtime.channel != prev_channel {
        let c = runtime.channel;
        state.config.update(|cfg| cfg.last_channel = c);
    }

    drop(runtime);
    state.emit(app);
}

/// Owns the currently-running reader thread's stop flag so switching ports
/// (or the manual picker retrying) cleanly tears down the previous one.
pub struct SerialManager {
    stop_current: Mutex<Option<Arc<AtomicBool>>>,
}

impl SerialManager {
    pub fn new() -> Self {
        Self {
            stop_current: Mutex::new(None),
        }
    }

    pub fn connect(&self, app: AppHandle, state: Arc<AppState>, port_name: String) -> Result<(), String> {
        let port = serialport::new(&port_name, BAUD_RATE)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|e| format!("couldn't open {port_name}: {e}"))?;

        // stop whatever reader is currently running before starting a new one
        if let Some(prev) = self.stop_current.lock().unwrap_or_else(|e| e.into_inner()).take() {
            prev.store(true, Ordering::Relaxed);
        }
        let stop = Arc::new(AtomicBool::new(false));
        *self.stop_current.lock().unwrap_or_else(|e| e.into_inner()) = Some(stop.clone());

        state.set_connected(true);
        state.config.update(|c| c.last_port = Some(port_name.clone()));
        state.emit(&app);

        thread::Builder::new()
            .name("serial-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(port);
                let mut nav = NavState::new();
                let mut was_down = false;
                // raw bytes, not read_line: read_line errors on any non-UTF-8
                // byte, and the junk an Arduino sends while the port opens or
                // the board resets would drop the connection
                let mut buf: Vec<u8> = Vec::with_capacity(64);
                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match reader.read_until(b'\n', &mut buf) {
                        Ok(0) => break, // port closed (device unplugged)
                        Ok(_) => {
                            if buf.last() == Some(&b'\n') {
                                let line = String::from_utf8_lossy(&buf);
                                if let Some(frame) = parse_line(&line) {
                                    process_frame(&app, &state, &mut nav, &mut was_down, frame);
                                }
                                buf.clear();
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::Interrupted => {
                            // a partial line survives a timeout; noise without
                            // newlines can't grow the buffer forever
                            if buf.len() > 256 {
                                buf.clear();
                            }
                            continue;
                        }
                        Err(_) => break,
                    }
                }
                if !stop.load(Ordering::Relaxed) {
                    // thread is exiting because of a real error/unplug, not
                    // because a newer connect() superseded it
                    state.set_connected(false);
                    state.emit(&app);
                }
            })
            .map_err(|e| e.to_string())?;

        Ok(())
    }
}

pub fn list_ports() -> Vec<String> {
    serialport::available_ports()
        .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{AudioBackend, AudioSessionInfo};
    use crate::config::ConfigStore;

    fn f(x: XAxis, y: YAxis) -> HwFrame {
        HwFrame { k: [50, 50, 50, 50], x, y, btn: false }
    }
    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    #[test]
    fn pull_down_fires_once_per_pull() {
        let mut d = false;
        let n = f(XAxis::Neutral, YAxis::Neutral);
        let down = f(XAxis::Neutral, YAxis::Down);
        assert!(!step_pull(&mut d, &n));
        assert!(step_pull(&mut d, &down), "pull fires");
        assert!(!step_pull(&mut d, &down), "holding doesn't repeat");
        assert!(!step_pull(&mut d, &f(XAxis::Left, YAxis::Down)), "still down");
        assert!(!step_pull(&mut d, &f(XAxis::Neutral, YAxis::Up)), "up does nothing");
        assert!(step_pull(&mut d, &down), "next pull fires again");
    }

    #[test]
    fn parses_good_lines_and_drops_torn_ones() {
        let fr = parse_line("34,78,12,55,NEUTRAL,UP,0\r\n").expect("valid line");
        assert_eq!(fr.k, [34, 78, 12, 55]);
        assert_eq!(fr.x, XAxis::Neutral);
        assert_eq!(fr.y, YAxis::Up);
        assert!(parse_line("34,78,12,55,NEUTRAL,UP").is_none(), "6 fields");
        assert!(parse_line("34,78,12,55,NEUTRAL,UP,0,9").is_none(), "8 fields");
        assert!(parse_line("34,78,1").is_none(), "torn line");
        assert!(parse_line("101,0,0,0,NEUTRAL,NEUTRAL,0").is_none(), "knob over 100");
        assert!(parse_line("0,0,0,0,SIDEWAYS,NEUTRAL,0").is_none(), "bad x label");
        assert!(parse_line("0,0,0,0,NEUTRAL,NEUTRAL,2").is_none(), "bad button");
    }

    #[test]
    fn nav_is_left_right_only_and_repeats() {
        let mut nav = NavState::new();
        let t0 = Instant::now();
        assert_eq!(step_nav(&mut nav, &f(XAxis::Neutral, YAxis::Up), t0), None, "up/down do nothing");
        assert_eq!(step_nav(&mut nav, &f(XAxis::Right, YAxis::Neutral), ms(t0, 50)), Some(Dir::NextChannel));
        assert_eq!(step_nav(&mut nav, &f(XAxis::Right, YAxis::Neutral), ms(t0, 100)), None);
        assert_eq!(step_nav(&mut nav, &f(XAxis::Right, YAxis::Neutral), ms(t0, 400)), Some(Dir::NextChannel), "repeat");
        assert_eq!(step_nav(&mut nav, &f(XAxis::Left, YAxis::Neutral), ms(t0, 450)), Some(Dir::PrevChannel));
    }


    #[derive(Clone, Default)]
    struct MuteLog(Arc<Mutex<Vec<(String, bool)>>>);
    impl AudioBackend for MuteLog {
        fn list_sessions(&self) -> Vec<AudioSessionInfo> { vec![] }
        fn set_volume(&self, _: &str, _: f32) {}
        fn set_mute(&self, id: &str, m: bool) {
            self.0.lock().unwrap().push((id.to_string(), m));
        }
    }

    #[test]
    fn startup_unmutes_apps_an_old_version_muted() {
        let dir = std::env::temp_dir().join(format!("vm2-test-{}-unmute", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::load(dir);
        store.update(|c| {
            c.banks[0][0].app_id = Some("brave".into());
            c.banks[0][1].app_id = Some("spotify".into());
            c.muted[0][1] = true;
        });
        let log = MuteLog::default();
        let state = AppState::new(store, Box::new(log.clone()));
        assert_eq!(*log.0.lock().unwrap(), vec![("spotify".to_string(), false)]);
        assert!(state.config.get().muted.iter().flatten().all(|m| !m));
    }
}
