//! Windows process-launch configuration.
//!
//! A GUI application that spawns a console program — `ffmpeg.exe`,
//! `taskkill.exe` — makes Windows allocate a console for it, and that console is
//! a window: it appears on screen, takes focus, and disappears when the child
//! exits. From the user's side it is a black rectangle flashing over whatever
//! they were doing, roughly once per download, and it looks like something going
//! wrong.
//!
//! `CREATE_NO_WINDOW` is what stops it. It suppresses the console allocation
//! only: the child runs exactly as before, with the same exit code, and its
//! stdout and stderr still arrive through the pipes the caller set up. That
//! last part matters — this module is deliberately *not* a reason to stop
//! capturing a tool's output, and the FFmpeg runner reads its diagnostics from
//! the very pipes that stay intact here.
//!
//! Every process this application launches goes through this trait. The one
//! place a visible console is left alone is the development workflow: when the
//! application is run from a terminal, that terminal belongs to the developer
//! and is not ours to hide.

/// Suppress the console window a spawned child would otherwise allocate.
///
/// Implemented for both the standard and the async `Command`, so a call site
/// does not have to care which one it holds. On every platform other than
/// Windows this is a no-op that returns the command unchanged.
pub trait NoConsoleWindow {
    /// Apply the platform's window-suppression setting to this command.
    fn no_console_window(&mut self) -> &mut Self;
}

/// `CREATE_NO_WINDOW`. The child gets no console at all, rather than a hidden
/// one: nothing is allocated, so nothing can be shown.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
impl NoConsoleWindow for std::process::Command {
    fn no_console_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(CREATE_NO_WINDOW)
    }
}

#[cfg(not(windows))]
impl NoConsoleWindow for std::process::Command {
    fn no_console_window(&mut self) -> &mut Self {
        self
    }
}

#[cfg(windows)]
impl NoConsoleWindow for tokio::process::Command {
    fn no_console_window(&mut self) -> &mut Self {
        self.creation_flags(CREATE_NO_WINDOW)
    }
}

#[cfg(not(windows))]
impl NoConsoleWindow for tokio::process::Command {
    fn no_console_window(&mut self) -> &mut Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Creation flags cannot be read back: `CommandExt::creation_flags` has no
    // stable getter, and `Command`'s `Debug` output does not include them. So
    // what is asserted here is the value being passed — which is where a typo
    // would live — and the effect is confirmed by the window-count check in
    // `tests/no_console_window.rs`, which runs the real thing.

    #[test]
    #[cfg(windows)]
    fn the_flag_is_the_documented_value() {
        // 0x08000000 is CREATE_NO_WINDOW and nothing else. A different flag
        // here would still compile and still suppress *something*, which is
        // exactly why the number is written out.
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
    }

    #[test]
    #[cfg(windows)]
    fn applying_it_leaves_the_command_usable() {
        let mut command = std::process::Command::new("cmd.exe");
        command.args(["/c", "echo probe"]);
        command.no_console_window();

        let output = command.output().expect("the command must still run");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "probe");
    }

    #[test]
    #[cfg(windows)]
    fn applying_it_to_an_async_command_leaves_it_usable() {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.args(["/c", "echo probe"]);
        command.no_console_window();

        let output = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(command.output())
            .expect("the command must still run");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "probe");
    }

    #[test]
    fn the_trait_is_a_no_op_off_windows() {
        // Compiles and behaves identically on every platform, so a call site
        // never needs its own `cfg` guard.
        #[cfg(not(windows))]
        {
            let mut command = std::process::Command::new("echo");
            assert!(std::ptr::eq(
                command.no_console_window() as *const _,
                &command as *const _
            ));
        }
    }
}
