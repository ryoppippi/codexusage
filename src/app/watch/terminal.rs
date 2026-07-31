//! Terminal lifecycle and capability checks for watch mode.

use eyre::{Result, WrapErr};
use std::io::{self, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;

/// Enter the alternate screen and hide the cursor.
const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?25l";
/// Show the cursor and leave the alternate screen.
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?25h\x1b[?1049l";

#[cfg(unix)]
/// Unix terminal input-mode management.
mod watch_input {
    use nix::sys::termios::{LocalFlags, SetArg, Termios, tcgetattr, tcsetattr};
    use std::io::{self, IsTerminal, Stdin};
    use std::os::fd::AsFd;

    /// Scoped terminal input mode for watch mode.
    pub(super) struct WatchInput<I: AsFd = Stdin> {
        /// Input handle whose mode was changed.
        input: I,
        /// Original terminal mode, or `None` for redirected input.
        original: Option<Termios>,
    }

    impl WatchInput<Stdin> {
        /// Disable echo on the process standard input when it is a terminal.
        pub(super) fn enter() -> io::Result<Self> {
            Self::enter_for(io::stdin())
        }
    }

    impl<I: AsFd> WatchInput<I> {
        /// Disable echo on one terminal input handle.
        pub(super) fn enter_for(input: I) -> io::Result<Self>
        where
            I: IsTerminal,
        {
            let original = input
                .is_terminal()
                .then(|| disable_echo(&input))
                .transpose()?;
            Ok(Self { input, original })
        }

        /// Restore the original mode and discard input queued during watch mode.
        pub(super) fn restore(&mut self) -> io::Result<()> {
            let Some(original) = self.original.as_ref() else {
                return Ok(());
            };

            tcsetattr(&self.input, SetArg::TCSAFLUSH, original).map_err(io::Error::from)?;
            self.original = None;
            Ok(())
        }

        /// Borrow the guarded input for terminal-mode assertions.
        #[cfg(test)]
        pub(super) fn input(&self) -> &I {
            &self.input
        }
    }

    impl<I: AsFd> Drop for WatchInput<I> {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    /// Save the current mode and disable only terminal echo flags.
    fn disable_echo(input: &impl AsFd) -> io::Result<Termios> {
        let original = tcgetattr(input).map_err(io::Error::from)?;
        let mut watch_mode = original.clone();
        watch_mode
            .local_flags
            .remove(LocalFlags::ECHO | LocalFlags::ECHONL);
        tcsetattr(input, SetArg::TCSANOW, &watch_mode).map_err(io::Error::from)?;
        Ok(original)
    }
}

#[cfg(windows)]
/// Windows console input-mode management.
mod watch_input {
    use std::ffi::c_void;
    use std::io::{self, IsTerminal};

    type Bool = i32;
    type Dword = u32;
    type Handle = *mut c_void;

    const ENABLE_ECHO_INPUT: Dword = 0x0004;
    const STD_INPUT_HANDLE: Dword = (-10_i32) as Dword;

    unsafe extern "system" {
        fn FlushConsoleInputBuffer(console_input: Handle) -> Bool;
        fn GetConsoleMode(console_handle: Handle, mode: *mut Dword) -> Bool;
        fn GetStdHandle(n_std_handle: Dword) -> Handle;
        fn SetConsoleMode(console_handle: Handle, mode: Dword) -> Bool;
    }

    /// Scoped Windows console input mode for watch mode.
    pub(super) struct WatchInput {
        /// Console input handle whose mode was changed.
        handle: Handle,
        /// Original console input mode, or `None` for redirected input.
        original: Option<Dword>,
    }

    impl WatchInput {
        /// Disable echo on the process standard input when it is a console.
        pub(super) fn enter() -> io::Result<Self> {
            if !io::stdin().is_terminal() {
                return Ok(Self {
                    handle: std::ptr::null_mut(),
                    original: None,
                });
            }

            unsafe {
                let handle = GetStdHandle(STD_INPUT_HANDLE);
                if handle.is_null() || handle as isize == -1 {
                    return Err(io::Error::last_os_error());
                }

                let mut original = 0;
                check_console_call(GetConsoleMode(handle, &raw mut original))?;
                check_console_call(SetConsoleMode(handle, original & !ENABLE_ECHO_INPUT))?;
                Ok(Self {
                    handle,
                    original: Some(original),
                })
            }
        }

        /// Restore the console mode and discard input queued during watch mode.
        pub(super) fn restore(&mut self) -> io::Result<()> {
            let Some(original) = self.original else {
                return Ok(());
            };

            unsafe {
                let restore_result = check_console_call(SetConsoleMode(self.handle, original));
                let flush_result = check_console_call(FlushConsoleInputBuffer(self.handle));
                let result = restore_result.and(flush_result);
                if result.is_ok() {
                    self.original = None;
                }
                result
            }
        }
    }

    impl Drop for WatchInput {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    /// Convert a Windows console return value into an I/O result.
    fn check_console_call(result: Bool) -> io::Result<()> {
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(any(unix, windows)))]
/// Input-mode fallback for unsupported terminal platforms.
mod watch_input {
    use std::io;

    /// No-op input guard for platforms without Unix terminals or Windows consoles.
    pub(super) struct WatchInput;

    impl WatchInput {
        /// Enter the platform's no-op input mode.
        pub(super) fn enter() -> io::Result<Self> {
            Ok(Self)
        }

        /// Restore the platform's no-op input mode.
        pub(super) fn restore(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

use watch_input::WatchInput;

/// Scoped access to watch-mode terminal output.
pub(super) struct WatchTerminal<'output, W: Write> {
    /// Terminal output borrowed for the lifetime of the alternate screen.
    output: &'output mut W,
    /// Terminal input mode scoped to the alternate-screen session.
    input: WatchInput,
    /// Whether terminal restoration is still required.
    active: bool,
}

impl<'output, W: Write> WatchTerminal<'output, W> {
    /// Enter watch mode on the supplied terminal output.
    pub(super) fn enter(output: &'output mut W) -> io::Result<Self> {
        let input = WatchInput::enter()?;
        let terminal = Self {
            output,
            input,
            active: true,
        };
        terminal.output.write_all(ENTER_ALTERNATE_SCREEN)?;
        terminal.output.flush()?;
        Ok(terminal)
    }

    /// Borrow the active terminal output for one redraw.
    pub(super) fn output(&mut self) -> &mut W {
        self.output
    }

    /// Restore the terminal and consume the session guard.
    pub(super) fn leave(mut self) -> io::Result<()> {
        self.restore()
    }

    /// Restore the terminal when it is still in watch mode.
    fn restore(&mut self) -> io::Result<()> {
        let output_result = if self.active {
            self.output
                .write_all(LEAVE_ALTERNATE_SCREEN)
                .and_then(|()| self.output.flush())
        } else {
            Ok(())
        };
        if output_result.is_ok() {
            self.active = false;
        }

        let input_result = self.input.restore();
        output_result.and(input_result)
    }
}

impl<W: Write> Drop for WatchTerminal<'_, W> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Receives cooperative watch-mode shutdown requests.
pub(super) struct WatchInterrupt {
    /// Bounded signal notification receiver owned by the watch thread.
    receiver: Receiver<()>,
}

impl WatchInterrupt {
    /// Install the process-wide Ctrl-C handler for watch mode.
    pub(super) fn install() -> Result<Self> {
        let (sender, receiver) = sync_channel(1);
        ctrlc::try_set_handler(move || notify_watch_interrupt(&sender))
            .wrap_err("failed to install Ctrl-C handler for watch mode")?;
        Ok(Self { receiver })
    }

    /// Wait until either shutdown is requested or the refresh interval elapses.
    pub(super) fn wait_timeout(&self, timeout: Duration) -> bool {
        match self.receiver.recv_timeout(timeout) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
            Err(RecvTimeoutError::Timeout) => false,
        }
    }
}

/// Queue one shutdown notification without blocking the signal-handler thread.
fn notify_watch_interrupt(sender: &SyncSender<()>) {
    let _ = sender.try_send(());
}

/// Decide whether the terminal should receive ANSI clear-screen sequences.
pub(in crate::app) fn supports_watch_screen_clear(term: Option<&str>) -> bool {
    supports_watch_screen_clear_with_platform(term, cfg!(windows), windows_stdout_supports_ansi())
}

/// Decide whether the terminal should receive ANSI clear-screen sequences.
pub(in crate::app) fn supports_watch_screen_clear_with_platform(
    term: Option<&str>,
    is_windows: bool,
    windows_stdout_supports_ansi: bool,
) -> bool {
    if term == Some("dumb") {
        return false;
    }

    if !is_windows {
        return true;
    }

    term.is_some() || windows_stdout_supports_ansi
}

/// Check whether the active Windows stdout console supports ANSI clear-screen sequences.
#[cfg(windows)]
fn windows_stdout_supports_ansi() -> bool {
    type Bool = i32;
    type Dword = u32;
    type Handle = *mut std::ffi::c_void;

    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: Dword = 0x0004;
    const STD_OUTPUT_HANDLE: Dword = (-11_i32) as Dword;

    unsafe extern "system" {
        fn GetStdHandle(n_std_handle: Dword) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut Dword) -> Bool;
        fn SetConsoleMode(handle: Handle, mode: Dword) -> Bool;
    }

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle as isize == -1 {
            return false;
        }

        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return true;
        }

        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

/// Non-Windows platforms rely on TERM and TTY detection instead of console probing.
#[cfg(not(windows))]
fn windows_stdout_supports_ansi() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    #[cfg(unix)]
    use nix::pty::openpty;
    #[cfg(unix)]
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    #[cfg(unix)]
    use std::fs::File;
    #[cfg(unix)]
    use std::io::Read;
    #[cfg(unix)]
    use std::os::fd::AsRawFd;

    #[test]
    fn watch_terminal_enters_and_leaves_alternate_screen() {
        let mut output = Vec::new();

        {
            let mut terminal = WatchTerminal::enter(&mut output).expect("enter terminal");
            terminal.output().write_all(b"frame").expect("write frame");
            terminal.leave().expect("leave terminal");
        }

        assert_eq!(output, b"\x1b[?1049h\x1b[?25lframe\x1b[?25h\x1b[?1049l");
    }

    #[test]
    fn watch_terminal_drop_restores_alternate_screen() {
        let mut output = Vec::new();

        {
            let _terminal = WatchTerminal::enter(&mut output).expect("enter terminal");
        }

        assert_eq!(output, b"\x1b[?1049h\x1b[?25l\x1b[?25h\x1b[?1049l");
    }

    #[cfg(unix)]
    #[test]
    fn watch_input_disables_only_echo_and_restores_original_mode() {
        let pty = openpty(None, None).expect("open PTY");
        let input = File::from(pty.slave);
        let mut configured = tcgetattr(&input).expect("read initial input mode");
        configured
            .local_flags
            .insert(LocalFlags::ECHO | LocalFlags::ECHONL | LocalFlags::ICANON | LocalFlags::ISIG);
        tcsetattr(&input, SetArg::TCSANOW, &configured).expect("configure PTY input mode");
        let original = tcgetattr(&input).expect("read configured input mode");

        let mut watch_input = WatchInput::enter_for(input).expect("enter watch input mode");
        let active = tcgetattr(watch_input.input()).expect("read active input mode");

        assert!(
            !active
                .local_flags
                .intersects(LocalFlags::ECHO | LocalFlags::ECHONL)
        );
        assert_eq!(
            active
                .local_flags
                .difference(LocalFlags::ECHO | LocalFlags::ECHONL),
            original
                .local_flags
                .difference(LocalFlags::ECHO | LocalFlags::ECHONL),
        );

        watch_input.restore().expect("restore watch input mode");
        assert_eq!(
            tcgetattr(watch_input.input()).expect("read restored input mode"),
            original,
        );
    }

    #[cfg(unix)]
    #[test]
    fn watch_input_drop_restores_mode_and_discards_queued_input() {
        let pty = openpty(None, None).expect("open PTY");
        let mut master = File::from(pty.master);
        let mut input = File::from(pty.slave);
        let guard_input = input.try_clone().expect("clone PTY input");
        let original = tcgetattr(&input).expect("read original input mode");

        let watch_input = WatchInput::enter_for(guard_input).expect("enter watch input mode");
        master
            .write_all(b"discarded\n")
            .expect("queue discarded input");
        wait_until_readable(&input);
        drop(watch_input);

        assert_eq!(
            tcgetattr(&input).expect("read restored input mode"),
            original,
        );
        master
            .write_all(b"retained\n")
            .expect("queue retained input");
        let mut received = [0_u8; 32];
        let received_len = input.read(&mut received).expect("read retained input");
        assert_eq!(&received[..received_len], b"retained\n");
    }

    #[cfg(unix)]
    fn wait_until_readable(input: &File) {
        let mut descriptor = nix::libc::pollfd {
            fd: input.as_raw_fd(),
            events: nix::libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` points to one initialized poll descriptor for the duration
        // of the call, and `input` keeps its file descriptor open.
        let ready = unsafe { nix::libc::poll(&raw mut descriptor, 1, 1_000) };
        assert_eq!(ready, 1, "PTY input did not become readable");
        assert_ne!(descriptor.revents & nix::libc::POLLIN, 0);
    }

    #[test]
    fn watch_interrupt_wait_distinguishes_refresh_from_shutdown() {
        let (sender, receiver) = sync_channel(1);
        let interrupt = WatchInterrupt { receiver };

        assert!(!interrupt.wait_timeout(Duration::ZERO));
        sender.try_send(()).expect("request shutdown");
        assert!(interrupt.wait_timeout(Duration::ZERO));
    }

    #[test]
    fn watch_interrupt_notification_is_bounded() {
        let (sender, receiver) = sync_channel(1);
        let interrupt = WatchInterrupt { receiver };

        notify_watch_interrupt(&sender);
        notify_watch_interrupt(&sender);

        assert!(interrupt.wait_timeout(Duration::ZERO));
        assert!(!interrupt.wait_timeout(Duration::ZERO));
    }
}
