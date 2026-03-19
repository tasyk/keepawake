//! KeepAwake: prevents Windows sleep via execution state. "On" also simulates mouse input
//! and can foreground Teams so Teams stays active. "Away" blocks sleep only so Teams can go Away.
//! Control via system tray.

#![windows_subsystem = "windows"]

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rand::Rng;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Icon as MenuIcon, IconMenuItemBuilder, Menu, MenuEvent, MenuId};
use tray_icon::{Icon, TrayIconBuilder};
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::Foundation::{BOOL, FALSE, HWND, LPARAM, TRUE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, LASTINPUTINFO, SendInput, INPUT, INPUT_0, INPUT_TYPE, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, INPUT_MOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, SetForegroundWindow,
};
use windows::Win32::System::SystemInformation::GetTickCount;

/// Teams goes Away after ~5 min of no keyboard/mouse activity (documented 300s).
const TEAMS_AWAY_TIMEOUT_SECS: u32 = 300;
/// When user idle time reaches this, we make Teams the active window once (so Teams doesn't go Away).
const IDLE_THRESHOLD_ACTIVATE_TEAMS_SECS: u32 = TEAMS_AWAY_TIMEOUT_SECS - 60; // 4 min
/// When user was active again (idle < this), reset so we can activate Teams again next idle period.
const IDLE_RESET_SECS: u32 = 60;
/// How often we check idle time (seconds).
const IDLE_CHECK_INTERVAL_SECS: u64 = 30;

const JITTER_PIXELS: i32 = 4;
const MOVE_INTERVAL_SECS: u64 = 60;

/// State passed to EnumWindows callback to capture the first Teams window found.
struct FindTeamsState {
    found: Option<HWND>,
}

unsafe extern "system" fn find_teams_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = (lparam.0 as *mut FindTeamsState).as_mut();
    let Some(state) = state else { return TRUE };
    if state.found.is_some() {
        return FALSE;
    }
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len <= 0 {
        return TRUE;
    }
    let title = String::from_utf16_lossy(&buf[..len as usize]);
    if title.to_uppercase().contains("TEAMS") {
        state.found = Some(hwnd);
        return FALSE;
    }
    TRUE
}

/// Finds a top-level window whose title contains "Teams" (e.g. Microsoft Teams).
fn find_teams_window() -> Option<HWND> {
    let mut state = FindTeamsState { found: None };
    unsafe {
        let _ = EnumWindows(Some(find_teams_callback), LPARAM(&mut state as *mut _ as isize));
    }
    state.found
}

/// Returns current user idle time (no keyboard/mouse) in seconds.
fn get_idle_secs() -> Option<u32> {
    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        if !GetLastInputInfo(&mut lii).as_bool() {
            return None;
        }
        let now = GetTickCount();
        let elapsed_ms = now.wrapping_sub(lii.dwTime);
        Some(elapsed_ms / 1000)
    }
}

/// Makes Teams the active window and leaves it there (no restore). Call when user idle time
/// is approaching Teams' Away timeout so Teams stays "active" and doesn't switch to Away.
fn activate_teams_and_leave() {
    let Some(teams_hwnd) = find_teams_window() else { return };
    unsafe {
        let _ = SetForegroundWindow(teams_hwnd);
    }
}

/// Sends a small relative mouse move via SendInput so that Windows (and Teams) registers
/// it as user activity (updates last input time); keeps Teams from switching to Away.
fn move_mouse_slightly() {
    let mut rng = rand::thread_rng();
    let dx: i32 = rng.gen_range(-JITTER_PIXELS..=JITTER_PIXELS);
    let dy: i32 = rng.gen_range(-JITTER_PIXELS..=JITTER_PIXELS);
    if dx == 0 && dy == 0 {
        return;
    }
    let input = INPUT {
        r#type: INPUT_TYPE(INPUT_MOUSE.0),
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSE_EVENT_FLAGS(0x0001), // MOUSEEVENTF_MOVE
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        let _ = SendInput(&[input], size_of::<INPUT>() as i32);
    }
}

/// Builds a simple 16x16 solid-color icon for context menu rows (muda `Icon`).
fn make_menu_icon_color(r: u8, g: u8, b: u8) -> Result<MenuIcon, tray_icon::menu::BadIcon> {
    const SZ: usize = 16;
    let mut rgba = vec![0u8; SZ * SZ * 4];
    for y in 0..SZ {
        for x in 0..SZ {
            let i = (y * SZ + x) * 4;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }
    MenuIcon::from_rgba(rgba, SZ as u32, SZ as u32)
}

/// Builds a simple 16x16 solid-color icon for the tray (R, G, B).
fn make_tray_icon_color(r: u8, g: u8, b: u8) -> Result<Icon, tray_icon::BadIcon> {
    const SZ: usize = 16;
    let mut rgba = vec![0u8; SZ * SZ * 4];
    for y in 0..SZ {
        for x in 0..SZ {
            let i = (y * SZ + x) * 4;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }
    Icon::from_rgba(rgba, SZ as u32, SZ as u32)
}

/// Tray-driven operating mode.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AwakeMode {
    /// No sleep prevention, no input simulation.
    Off,
    /// Block sleep + mouse jitter + Teams foreground when idle.
    On,
    /// Block sleep only; natural idle so Teams can show Away.
    Away,
}

impl AwakeMode {
    const fn to_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::Away => 2,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::On,
            2 => Self::Away,
            _ => Self::Off,
        }
    }
}

#[derive(Clone, Copy)]
enum TrayUserEvent {
    Quit,
    StateChanged(AwakeMode),
}

fn main() {
    let mode_atomic = Arc::new(std::sync::atomic::AtomicU8::new(AwakeMode::Off.to_u8()));
    let quit = Arc::new(AtomicBool::new(false));

    let event_loop = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let icon_gray = match make_tray_icon_color(100, 100, 120) {
        Ok(i) => i,
        Err(_) => {
            eprintln!("Failed to create tray icon");
            return;
        }
    };
    let icon_green = match make_tray_icon_color(80, 180, 100) {
        Ok(i) => i,
        Err(_) => {
            eprintln!("Failed to create tray icon");
            return;
        }
    };
    let icon_yellow = match make_tray_icon_color(240, 200, 60) {
        Ok(i) => i,
        Err(_) => {
            eprintln!("Failed to create tray icon");
            return;
        }
    };
    let on_menu_icon = match make_menu_icon_color(80, 180, 100) {
        Ok(i) => i,
        Err(_) => {
            eprintln!("Failed to create menu icon");
            return;
        }
    };
    let away_menu_icon = match make_menu_icon_color(255, 210, 40) {
        Ok(i) => i,
        Err(_) => {
            eprintln!("Failed to create menu icon");
            return;
        }
    };
    let off_menu_icon = match make_menu_icon_color(115, 115, 138) {
        Ok(i) => i,
        Err(_) => {
            eprintln!("Failed to create menu icon");
            return;
        }
    };
    let exit_menu_icon = match make_menu_icon_color(215, 80, 80) {
        Ok(i) => i,
        Err(_) => {
            eprintln!("Failed to create menu icon");
            return;
        }
    };

    let menu = Menu::new();
    let id_enable = MenuId::new("enable");
    let id_away = MenuId::new("away");
    let id_disable = MenuId::new("disable");
    let id_quit = MenuId::new("quit");

    // Keep menu items alive for the lifetime of the tray (required on Windows).
    let item_on = IconMenuItemBuilder::new()
        .text("Turn on")
        .id(id_enable.clone())
        .enabled(true)
        .icon(Some(on_menu_icon))
        .build();
    let item_away = IconMenuItemBuilder::new()
        .text("Away")
        .id(id_away.clone())
        .enabled(true)
        .icon(Some(away_menu_icon))
        .build();
    let item_off = IconMenuItemBuilder::new()
        .text("Turn off")
        .id(id_disable.clone())
        .enabled(true)
        .icon(Some(off_menu_icon))
        .build();
    let item_exit = IconMenuItemBuilder::new()
        .text("Exit")
        .id(id_quit.clone())
        .enabled(true)
        .icon(Some(exit_menu_icon))
        .build();

    menu.append(&item_on).unwrap();
    menu.append(&item_away).unwrap();
    menu.append(&item_off).unwrap();
    menu.append(&item_exit).unwrap();

    let tray = TrayIconBuilder::new()
        .with_tooltip("KeepAwake (off)")
        .with_icon(icon_gray.clone())
        .with_menu(Box::new(menu))
        .build()
        .expect("Failed to create tray icon");

    let proxy_ev = proxy.clone();
    let mode_ev = Arc::clone(&mode_atomic);
    let quit_ev = Arc::clone(&quit);
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if *event.id() == id_enable {
            mode_ev.store(AwakeMode::On.to_u8(), Ordering::Relaxed);
            let _ = proxy_ev.send_event(TrayUserEvent::StateChanged(AwakeMode::On));
        } else if *event.id() == id_away {
            mode_ev.store(AwakeMode::Away.to_u8(), Ordering::Relaxed);
            let _ = proxy_ev.send_event(TrayUserEvent::StateChanged(AwakeMode::Away));
        } else if *event.id() == id_disable {
            mode_ev.store(AwakeMode::Off.to_u8(), Ordering::Relaxed);
            let _ = proxy_ev.send_event(TrayUserEvent::StateChanged(AwakeMode::Off));
        } else if *event.id() == id_quit {
            quit_ev.store(true, Ordering::Relaxed);
            let _ = proxy_ev.send_event(TrayUserEvent::Quit);
        }
    }));

    let mode_worker = Arc::clone(&mode_atomic);
    let quit_worker = Arc::clone(&quit);
    thread::spawn(move || {
        let mut secs_since_move: u64 = 0;
        let mut secs_since_idle_check: u64 = 0;
        let mut teams_activated_this_idle: bool = false;
        let mut prev_mode = AwakeMode::Off;
        loop {
            if quit_worker.load(Ordering::Relaxed) {
                break;
            }
            let mode = AwakeMode::from_u8(mode_worker.load(Ordering::Relaxed));
            if mode == AwakeMode::On && prev_mode != AwakeMode::On {
                secs_since_move = 0;
                secs_since_idle_check = 0;
                teams_activated_this_idle = false;
            }
            prev_mode = mode;

            if mode == AwakeMode::On {
                secs_since_move += 1;
                secs_since_idle_check += 1;
                // Periodic mouse move so system/Teams see activity when user is active.
                if secs_since_move >= MOVE_INTERVAL_SECS {
                    secs_since_move = 0;
                    move_mouse_slightly();
                }
                // When idle time approaches Teams' Away timeout (~5 min), make Teams active once and leave it there.
                if secs_since_idle_check >= IDLE_CHECK_INTERVAL_SECS {
                    secs_since_idle_check = 0;
                    if let Some(idle_secs) = get_idle_secs() {
                        if idle_secs < IDLE_RESET_SECS {
                            teams_activated_this_idle = false;
                        } else if idle_secs >= IDLE_THRESHOLD_ACTIVATE_TEAMS_SECS && !teams_activated_this_idle {
                            activate_teams_and_leave();
                            teams_activated_this_idle = true;
                        }
                    }
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
    });

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let tao::event::Event::UserEvent(ev) = event {
            match ev {
                TrayUserEvent::Quit => {
                    quit.store(true, Ordering::Relaxed);
                    *control_flow = ControlFlow::Exit;
                }
                TrayUserEvent::StateChanged(mode) => {
                    match mode {
                        AwakeMode::Off => {
                            let _ = tray.set_icon(Some(icon_gray.clone()));
                            let _ = tray.set_tooltip(Some("KeepAwake (off)"));
                            unsafe {
                                let _ = SetThreadExecutionState(ES_CONTINUOUS);
                            }
                        }
                        AwakeMode::On => {
                            let _ = tray.set_icon(Some(icon_green.clone()));
                            let _ = tray.set_tooltip(Some("KeepAwake (on — Teams active)"));
                            unsafe {
                                let _ = SetThreadExecutionState(
                                    ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED | ES_CONTINUOUS,
                                );
                            }
                        }
                        AwakeMode::Away => {
                            let _ = tray.set_icon(Some(icon_yellow.clone()));
                            let _ = tray.set_tooltip(Some("KeepAwake (away — sleep blocked)"));
                            // Same power policy as On; no SendInput / Teams logic in worker thread.
                            unsafe {
                                let _ = SetThreadExecutionState(
                                    ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED | ES_CONTINUOUS,
                                );
                            }
                        }
                    }
                }
            }
        }
    });
}
