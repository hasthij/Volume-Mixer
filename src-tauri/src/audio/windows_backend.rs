//! Windows Core Audio (WASAPI session) backend, per-app volume/mute via
//! ISimpleAudioVolume. All COM work happens on one dedicated STA thread —
//! COM apartment/thread affinity means these interfaces can't just be
//! shared across whatever thread a Tauri command happens to run on, so
//! everything is funneled through a channel instead.
//!

use super::{AudioBackend, AudioSessionInfo};
use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, BOOL};
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

enum Cmd {
    List(Sender<Vec<AudioSessionInfo>>),
    SetVolume(String, f32),
    SetMute(String, bool),
}

pub struct WindowsAudioBackend {
    tx: Sender<Cmd>,
}

impl WindowsAudioBackend {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        thread::Builder::new()
            .name("audio-com".into())
            .spawn(move || audio_thread(rx))
            .expect("failed to spawn audio COM thread");
        Self { tx }
    }
}

impl AudioBackend for WindowsAudioBackend {
    fn list_sessions(&self) -> Vec<AudioSessionInfo> {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(Cmd::List(reply_tx)).is_err() {
            return Vec::new();
        }
        // never block the caller for long: this answers the overlay's app
        // menu, and a stalled audio thread must not freeze the UI with it
        reply_rx.recv_timeout(Duration::from_secs(3)).unwrap_or_default()
    }

    fn set_volume(&self, id: &str, volume01: f32) {
        let _ = self
            .tx
            .send(Cmd::SetVolume(id.to_string(), volume01.clamp(0.0, 1.0)));
    }

    fn set_mute(&self, id: &str, muted: bool) {
        let _ = self.tx.send(Cmd::SetMute(id.to_string(), muted));
    }
}

/// Some apps (Discord notably) run more than one simultaneous audio
/// session under the same process — e.g. a voice-call stream and a
/// separate UI-sounds stream — so a single exe id can map to several live
/// `ISimpleAudioVolume`s that all need to move together.
type Cache = HashMap<String, Vec<ISimpleAudioVolume>>;

/// How often the session list is re-scanned. A session that gets torn
/// down and recreated (Discord does this constantly — its audio session
/// comes and goes with voice calls) leaves behind a stale, silently
/// no-op'ing COM reference, and a newly started app has no entry at all;
/// a periodic re-scan picks both up. Scanning is far too slow to do per
/// command: the serial thread sends a volume for every knob at 20 Hz, so
/// re-scanning on each one (which is what used to happen for any assigned
/// app that wasn't running) let the queue grow without bound — volume
/// changes landed up to a minute late and the app menu froze the UI.
const RESCAN_EVERY: Duration = Duration::from_millis(1500);

fn audio_thread(rx: mpsc::Receiver<Cmd>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let mut cache: Cache = HashMap::new();
    let mut scanned_at: Option<Instant> = None;
    // what the knobs / mute state want, per app, and what was last applied
    let mut want_vol: HashMap<String, f32> = HashMap::new();
    let mut want_mute: HashMap<String, bool> = HashMap::new();
    let mut applied_vol: HashMap<String, f32> = HashMap::new();
    let mut applied_mute: HashMap<String, bool> = HashMap::new();

    loop {
        // wait for work, but wake up anyway to re-scan now and then so a
        // restarted app gets its volume back even if no knob moves
        let first = match rx.recv_timeout(RESCAN_EVERY) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // coalesce: drain everything queued and keep only the latest value
        // per app, so a burst of knob frames costs one COM call, not dozens
        let mut lists = Vec::new();
        for cmd in first.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            match cmd {
                Cmd::List(reply) => lists.push(reply),
                Cmd::SetVolume(id, vol) => {
                    want_vol.insert(id, vol);
                }
                Cmd::SetMute(id, muted) => {
                    want_mute.insert(id, muted);
                }
            }
        }

        let due = scanned_at.map_or(true, |t| t.elapsed() >= RESCAN_EVERY);
        if !lists.is_empty() || due {
            let sessions = unsafe { list_all_sessions(&mut cache) };
            scanned_at = Some(Instant::now());
            // fresh COM references: re-apply everything to them
            applied_vol.clear();
            applied_mute.clear();
            for reply in lists {
                let _ = reply.send(sessions.clone());
            }
        }

        for (id, &vol) in &want_vol {
            if applied_vol.get(id) == Some(&vol) {
                continue;
            }
            if let Some(list) = cache.get(id) {
                for v in list {
                    let _ = unsafe { v.SetMasterVolume(vol, std::ptr::null()) };
                }
                applied_vol.insert(id.clone(), vol);
            }
        }
        for (id, &muted) in &want_mute {
            if applied_mute.get(id) == Some(&muted) {
                continue;
            }
            if let Some(list) = cache.get(id) {
                for v in list {
                    let _ = unsafe { v.SetMute(BOOL::from(muted), std::ptr::null()) };
                }
                applied_mute.insert(id.clone(), muted);
            }
        }
    }

    unsafe { CoUninitialize() };
}

unsafe fn get_session_manager() -> windows::core::Result<IAudioSessionManager2> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
    device.Activate(CLSCTX_ALL, None)
}

unsafe fn list_all_sessions(cache: &mut Cache) -> Vec<AudioSessionInfo> {
    cache.clear();
    let mut out = Vec::new();
    let Ok(manager) = get_session_manager() else {
        return out;
    };
    let Ok(session_enum) = manager.GetSessionEnumerator() else {
        return out;
    };
    let Ok(count) = session_enum.GetCount() else {
        return out;
    };

    for i in 0..count {
        let Ok(control) = session_enum.GetSession(i) else {
            continue;
        };
        let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
            continue;
        };
        let Ok(pid) = control2.GetProcessId() else {
            continue;
        };
        if pid == 0 {
            continue; // system sounds session — nothing to attribute to an app
        }
        let Some(exe_stem) = process_exe_stem(pid) else {
            continue;
        };
        let Ok(simple_volume) = control.cast::<ISimpleAudioVolume>() else {
            continue;
        };

        let id = exe_stem.to_lowercase();
        if !cache.contains_key(&id) {
            let display_name = display_name_for(&control2, &exe_stem);
            out.push(AudioSessionInfo {
                id: id.clone(),
                display_name,
            });
        }
        cache.entry(id).or_default().push(simple_volume);
    }
    out
}

unsafe fn process_exe_stem(pid: u32) -> Option<String> {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    let result = QueryFullProcessImageNameW(
        handle,
        PROCESS_NAME_WIN32,
        windows::core::PWSTR(buf.as_mut_ptr()),
        &mut len,
    );
    let _ = CloseHandle(handle);
    if result.is_err() {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let stem = std::path::Path::new(&path)
        .file_stem()?
        .to_string_lossy()
        .to_string();
    Some(stem)
}

unsafe fn display_name_for(control2: &IAudioSessionControl2, exe_stem: &str) -> String {
    if let Ok(pwstr) = control2.GetDisplayName() {
        if !pwstr.is_null() {
            if let Ok(s) = pwstr.to_string() {
                CoTaskMemFree(Some(pwstr.0 as *const _));
                if !s.is_empty() && !s.starts_with('@') {
                    return s;
                }
            }
        }
    }
    title_case(exe_stem)
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}
