# Handoff — 2026-11-18
- Circle gesture: four quarter-turns of the stick in a row (either direction) open or close the overlay. Each turn has 600 ms. Logic: `step_gesture` in `src-tauri/src/serial.rs`.
- Closing with a circle puts the channel back where the circle started. Nav is paused until the stick returns to center.
- Removed mute, solo, banks and the button hold. Nav is left/right only. The button does nothing.
- On startup, apps that an old version muted are unmuted and `muted` in the config is cleared.
- The config still stores banks and muted, and the payload still has `muted`/`solo`/`hold`, so the frontend doesn't break.
- cargo test passes 6/6. The NSIS installer is built and installed silently to %LOCALAPPDATA%\Volume Mixer 2.0, and the app is running.
- Not done: the frontend in `dist/` still shows the bank UI, mute/solo icons and the hold ring, because reading `dist/` is denied.
- Not verified on the real Arduino. Not committed or pushed (waiting for the user to confirm).
