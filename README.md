# KeepAwake

Small Windows tray app that slightly moves the mouse cursor at intervals to prevent the system from going to sleep. You can turn the behaviour on or off from the system tray.

## Build

```bash
cargo build --release
```

The executable will be at `target/release/keepawake.exe`.

## Run

Double-click `keepawake.exe` or run from a terminal. An icon appears in the system tray (near the clock).

- **Turn on** — start moving the mouse slightly every 60 seconds.
- **Turn off** — stop moving the mouse.
- **Exit** — quit the application.

## Behaviour

- When "Turn on" is active, the cursor is shifted by a small random offset (up to ±4 pixels) every 60 seconds. This mimics light human activity so Windows does not enter sleep.
- The movement is minimal and should not interfere with normal use.

## Optional: run at Windows startup

1. Press `Win + R`, type `shell:startup`, press Enter.
2. Create a shortcut to `keepawake.exe` and place it in the opened folder.
