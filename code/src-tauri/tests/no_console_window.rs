//! Does a launched console program put a window on screen?
//!
//! The complaint is visual: a black window flashes when a download starts or a
//! profile is loaded. The cause is that `ffmpeg.exe` and `taskkill.exe` are
//! console programs, and a GUI application that starts one without
//! `CREATE_NO_WINDOW` makes Windows give it a console — and a console is a
//! window.
//!
//! Creation flags cannot be read back on stable Rust, so this does not inspect
//! the command. It watches the desktop instead, which is the thing the user
//! sees. Windows 11 may hand a new console to Windows Terminal rather than
//! creating an old-style console window, so this looks for *any* new visible
//! top-level window while the child runs and prints what it found — the control
//! case has to show something, or the check proves nothing.
//!
//! Ignored by default, because the control deliberately makes a window appear
//! and because it needs a desktop session to mean anything:
//!
//! ```text
//! cargo test --test no_console_window -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use douyin_downloader_lib::process::NoConsoleWindow;

/// `CREATE_NEW_CONSOLE`, used only as the control.
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

#[link(name = "user32")]
extern "system" {
    fn EnumWindows(callback: extern "system" fn(isize, isize) -> i32, parameter: isize) -> i32;
    fn GetClassNameW(window: isize, buffer: *mut u16, length: i32) -> i32;
    fn IsWindowVisible(window: isize) -> i32;
    fn GetWindowThreadProcessId(window: isize, process: *mut u32) -> u32;
}

static FOUND: Mutex<Option<BTreeSet<(String, u32)>>> = Mutex::new(None);

extern "system" fn collect(window: isize, _: isize) -> i32 {
    if unsafe { IsWindowVisible(window) } == 0 {
        return 1;
    }
    let mut buffer = [0u16; 128];
    let length = unsafe { GetClassNameW(window, buffer.as_mut_ptr(), buffer.len() as i32) };
    if length <= 0 {
        return 1;
    }
    let class = String::from_utf16_lossy(&buffer[..length as usize]);
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(window, &mut pid) };
    if let Ok(mut found) = FOUND.lock() {
        if let Some(set) = found.as_mut() {
            set.insert((class, pid));
        }
    }
    1
}

/// Every visible top-level window on this desktop: its class and owning process.
fn windows() -> BTreeSet<(String, u32)> {
    if let Ok(mut found) = FOUND.lock() {
        *found = Some(BTreeSet::new());
    }
    unsafe { EnumWindows(collect, 0) };
    let taken = FOUND
        .lock()
        .ok()
        .and_then(|mut found| found.take())
        .unwrap_or_default();
    taken
}

/// What appeared on the desktop while `spawn`'s child was running.
///
/// Polled rather than sampled once: a window appears shortly after the process
/// starts and disappears when it exits, and a single reading could land on
/// either side of that and see nothing.
fn windows_appearing_while(spawn: impl FnOnce() -> std::process::Child) -> BTreeSet<(String, u32)> {
    let before = windows();
    let mut child = spawn();
    let mut seen = before.clone();

    let deadline = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < deadline {
        seen.extend(windows());
        std::thread::sleep(Duration::from_millis(40));
    }

    let _ = child.kill();
    let _ = child.wait();
    seen.difference(&before).cloned().collect()
}

/// Start `cmd.exe` that lives long enough to be observed.
fn console_command() -> std::process::Command {
    let mut command = std::process::Command::new("cmd.exe");
    command
        .args(["/c", "ping -n 4 127.0.0.1"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

#[test]
#[ignore = "makes a window appear on purpose; run it explicitly"]
fn a_console_child_through_this_application_leaves_no_window_on_screen() {
    let baseline = windows();
    println!("visible top-level windows before: {}", baseline.len());

    // The control: an explicit console, which must be visible somewhere — a
    // console window, or a Windows Terminal window it was handed to.
    let control = windows_appearing_while(|| {
        use std::os::windows::process::CommandExt;
        let mut command = console_command();
        command.creation_flags(CREATE_NEW_CONSOLE);
        command.spawn().expect("cmd.exe must start")
    });
    println!(
        "control (CREATE_NEW_CONSOLE) produced {} new window(s):",
        control.len()
    );
    for (class, pid) in &control {
        println!("  class={class} pid={pid}");
    }
    assert!(
        !control.is_empty(),
        "the control produced no window at all, so this session cannot observe \
         console windows and the check below would prove nothing"
    );

    // The fix.
    let ours = windows_appearing_while(|| {
        let mut command = console_command();
        command.no_console_window();
        command.spawn().expect("cmd.exe must start")
    });
    println!(
        "through NoConsoleWindow produced {} new window(s):",
        ours.len()
    );
    for (class, pid) in &ours {
        println!("  class={class} pid={pid}");
    }

    // A Windows Terminal *tab* in an already-running window is not a new
    // top-level window and would not appear here; what this rules out is the
    // application causing a window to be created, which is the flash.
    assert!(
        ours.is_empty(),
        "a window appeared for a child launched by this application: {ours:?}"
    );
}
