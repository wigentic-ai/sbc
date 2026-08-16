use std::io::{self, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use portable_pty::{PtySize, native_pty_system};

use crate::clipboard::ClipboardContent;
use crate::command::SessionCommand;
use crate::target::Target;
use crate::{Result, clipboard, transfer};

const CTRL_V: u8 = 0x16;
const ESCAPE: u8 = 0x1b;
const MAX_ESCAPE_SEQUENCE: usize = 256;
const BRACKETED_PASTE_ENABLE: &[u8] = b"\x1b[?2004h";
const BRACKETED_PASTE_DISABLE: &[u8] = b"\x1b[?2004l";
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

enum ClipboardRequest {
    Warmup,
    Paste,
    Shutdown,
}

pub(crate) fn run(command: SessionCommand, target: Target, clipboard_enabled: bool) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("sbc requires an interactive terminal".into());
    }

    let (columns, rows) = size().unwrap_or((80, 24));
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols: columns,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut child = pair.slave.spawn_command(command.builder())?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
    let master = Arc::new(Mutex::new(pair.master));
    let done = Arc::new(AtomicBool::new(false));
    let bracketed_paste = Arc::new(AtomicBool::new(false));

    let raw_mode = RawModeGuard::enable()?;
    install_signal_restore();

    // Clipboard reads and sandbox uploads can take seconds. Keep them off the
    // input relay so terminal capability replies and keystrokes keep flowing.
    let (clipboard_sender, clipboard_receiver) = mpsc::channel();
    let clipboard_writer = Arc::clone(&writer);
    let clipboard_target = target.clone();
    let clipboard_done = Arc::clone(&done);
    let clipboard_bracketed_paste = Arc::clone(&bracketed_paste);
    let clipboard_thread = thread::spawn(move || {
        clipboard_loop(
            clipboard_writer,
            clipboard_target,
            clipboard_done,
            clipboard_bracketed_paste,
            clipboard_receiver,
        )
    });

    let input_writer = Arc::clone(&writer);
    let input_clipboard_sender = clipboard_sender.clone();
    let (input_ready_sender, input_ready_receiver) = mpsc::sync_channel(0);
    thread::spawn(move || {
        input_loop(
            input_writer,
            input_clipboard_sender,
            clipboard_enabled,
            input_ready_sender,
        )
    });

    let output_bracketed_paste = Arc::clone(&bracketed_paste);
    let output_thread = thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        let _ = relay_output_after_input_ready(
            &mut reader,
            &mut stdout,
            &output_bracketed_paste,
            input_ready_receiver,
        );
    });

    let resize_master = Arc::clone(&master);
    let resize_done = Arc::clone(&done);
    thread::spawn(move || resize_loop(resize_master, resize_done, (columns, rows)));

    let status = child.wait()?;
    done.store(true, Ordering::Relaxed);
    let _ = clipboard_sender.send(ClipboardRequest::Shutdown);
    let _ = clipboard_thread.join();
    drop(writer);
    drop(master);
    let _ = output_thread.join();
    drop(raw_mode);

    if !status.success() {
        return Err(format!("session {status}").into());
    }
    Ok(())
}

fn relay_output_after_input_ready<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    bracketed_paste: &AtomicBool,
    input_ready: mpsc::Receiver<()>,
) -> io::Result<()> {
    // Some TUIs query terminal capabilities during their first few milliseconds.
    // Do not expose those queries until the input relay can consume the outer
    // terminal's immediate replies.
    input_ready
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "input relay stopped"))?;
    let mut buffer = [0_u8; 8192];
    let mut terminal_modes = TerminalModes::default();
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        terminal_modes.observe(&buffer[..count], bracketed_paste);
        writer.write_all(&buffer[..count])?;
        writer.flush()?;
    }
}

#[derive(Default)]
struct TerminalModes {
    pending: Vec<u8>,
}

impl TerminalModes {
    fn observe(&mut self, input: &[u8], bracketed_paste: &AtomicBool) {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(input);

        for sequence in bytes.windows(BRACKETED_PASTE_ENABLE.len()) {
            if sequence == BRACKETED_PASTE_ENABLE {
                bracketed_paste.store(true, Ordering::Relaxed);
            } else if sequence == BRACKETED_PASTE_DISABLE {
                bracketed_paste.store(false, Ordering::Relaxed);
            }
        }

        let keep = BRACKETED_PASTE_ENABLE.len().saturating_sub(1);
        self.pending
            .extend_from_slice(&bytes[bytes.len().saturating_sub(keep)..]);
    }
}

fn input_loop(
    writer: SharedWriter,
    clipboard_sender: mpsc::Sender<ClipboardRequest>,
    clipboard_enabled: bool,
    ready: mpsc::SyncSender<()>,
) {
    let mut stdin = io::stdin().lock();
    if ready.send(()).is_err() {
        return;
    }
    let mut buffer = [0_u8; 1024];
    let mut relay = InputRelay::default();
    loop {
        let count = match stdin.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        let paste_count = {
            let mut writer = writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Ok(paste_count) = relay.forward(&mut *writer, &buffer[..count], clipboard_enabled)
            else {
                return;
            };
            if writer.flush().is_err() {
                return;
            }
            paste_count
        };
        if relay.take_terminal_response()
            && clipboard_sender.send(ClipboardRequest::Warmup).is_err()
        {
            return;
        }
        for _ in 0..paste_count {
            if clipboard_sender.send(ClipboardRequest::Paste).is_err() {
                return;
            }
        }
    }
}

#[derive(Default)]
struct InputRelay {
    pending: Vec<u8>,
    terminal_response: bool,
}

impl InputRelay {
    fn forward<W: Write>(
        &mut self,
        writer: &mut W,
        input: &[u8],
        clipboard_enabled: bool,
    ) -> io::Result<usize> {
        if !clipboard_enabled {
            if !self.pending.is_empty() {
                writer.write_all(&self.pending)?;
                self.pending.clear();
            }
            writer.write_all(input)?;
            return Ok(0);
        }

        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(input);
        let mut plain_start = 0;
        let mut cursor = 0;
        let mut paste_count = 0;

        while cursor < bytes.len() {
            if bytes[cursor] == ESCAPE && matches!(bytes.get(cursor + 1), Some(b'v' | b'V')) {
                writer.write_all(&bytes[plain_start..cursor])?;
                paste_count += 1;
                cursor += 2;
                plain_start = cursor;
                continue;
            }

            if bytes[cursor] == CTRL_V {
                writer.write_all(&bytes[plain_start..cursor])?;
                paste_count += 1;
                cursor += 1;
                plain_start = cursor;
                continue;
            }

            if bytes[cursor] == ESCAPE && bytes.get(cursor + 1) == Some(&b']') {
                let Some(sequence_end) = osc_sequence_end(&bytes[cursor..]) else {
                    if bytes.len() - cursor > MAX_ESCAPE_SEQUENCE {
                        cursor += 1;
                        continue;
                    }
                    writer.write_all(&bytes[plain_start..cursor])?;
                    self.pending.extend_from_slice(&bytes[cursor..]);
                    return Ok(paste_count);
                };
                let sequence_end = cursor + sequence_end;
                writer.write_all(&bytes[plain_start..cursor])?;
                self.terminal_response = true;
                cursor = sequence_end;
                plain_start = cursor;
                continue;
            }

            if bytes[cursor] != ESCAPE || bytes.get(cursor + 1) != Some(&b'[') {
                cursor += 1;
                continue;
            }

            let Some(sequence_end) = csi_sequence_end(&bytes[cursor..]) else {
                if bytes.len() - cursor > MAX_ESCAPE_SEQUENCE {
                    cursor += 1;
                    continue;
                }
                writer.write_all(&bytes[plain_start..cursor])?;
                self.pending.extend_from_slice(&bytes[cursor..]);
                return Ok(paste_count);
            };
            let sequence_end = cursor + sequence_end;
            match clipboard_shortcut_sequence(&bytes[cursor..sequence_end]) {
                ShortcutSequence::Press => {
                    writer.write_all(&bytes[plain_start..cursor])?;
                    paste_count += 1;
                    plain_start = sequence_end;
                }
                ShortcutSequence::Release => {
                    writer.write_all(&bytes[plain_start..cursor])?;
                    plain_start = sequence_end;
                }
                ShortcutSequence::Other if is_terminal_response(&bytes[cursor..sequence_end]) => {
                    writer.write_all(&bytes[plain_start..cursor])?;
                    self.terminal_response = true;
                    plain_start = sequence_end;
                }
                ShortcutSequence::Other => {}
            }
            cursor = sequence_end;
        }

        writer.write_all(&bytes[plain_start..])?;
        Ok(paste_count)
    }

    fn take_terminal_response(&mut self) -> bool {
        std::mem::take(&mut self.terminal_response)
    }
}

fn csi_sequence_end(input: &[u8]) -> Option<usize> {
    debug_assert!(input.starts_with(b"\x1b["));
    input[2..]
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|index| index + 3)
}

fn osc_sequence_end(input: &[u8]) -> Option<usize> {
    debug_assert!(input.starts_with(b"\x1b]"));
    let mut cursor = 2;
    while cursor < input.len() {
        if input[cursor] == 0x07 {
            return Some(cursor + 1);
        }
        if input[cursor] == ESCAPE && input.get(cursor + 1) == Some(&b'\\') {
            return Some(cursor + 2);
        }
        cursor += 1;
    }
    None
}

fn is_terminal_response(sequence: &[u8]) -> bool {
    let Some((&final_byte, body)) = sequence.split_last() else {
        return false;
    };
    let Some(parameters) = body.strip_prefix(b"\x1b[") else {
        return false;
    };
    match final_byte {
        b'c' => matches!(parameters.first(), Some(b'?' | b'>')),
        b'u' => parameters.first() == Some(&b'?'),
        b'R' | b't' => parameters.first().is_some_and(u8::is_ascii_digit),
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShortcutSequence {
    Press,
    Release,
    Other,
}

fn clipboard_shortcut_sequence(sequence: &[u8]) -> ShortcutSequence {
    let Some((&final_byte, body)) = sequence.split_last() else {
        return ShortcutSequence::Other;
    };
    let Some(parameters) = body.strip_prefix(b"\x1b[") else {
        return ShortcutSequence::Other;
    };

    match final_byte {
        // Kitty's keyboard protocol, used by current Windows Terminal.
        b'u' => kitty_clipboard_shortcut(parameters),
        // XTerm's modifyOtherKeys encoding, used by some nested terminals.
        b'~' => modify_other_keys_clipboard_shortcut(parameters),
        _ => ShortcutSequence::Other,
    }
}

fn kitty_clipboard_shortcut(parameters: &[u8]) -> ShortcutSequence {
    let mut fields = parameters.split(|byte| *byte == b';');
    if !is_v_codepoint(parameter_number(fields.next())) {
        return ShortcutSequence::Other;
    }
    let Some(modifiers_and_event) = fields.next() else {
        return ShortcutSequence::Other;
    };
    let mut parts = modifiers_and_event.split(|byte| *byte == b':');
    if !is_clipboard_modifier(parameter_number(parts.next())) {
        return ShortcutSequence::Other;
    }

    match parameter_number(parts.next()).unwrap_or(1) {
        1 | 2 => ShortcutSequence::Press,
        3 => ShortcutSequence::Release,
        _ => ShortcutSequence::Other,
    }
}

fn modify_other_keys_clipboard_shortcut(parameters: &[u8]) -> ShortcutSequence {
    let mut fields = parameters.split(|byte| *byte == b';');
    if parameter_number(fields.next()) != Some(27)
        || !is_clipboard_modifier(parameter_number(fields.next()))
        || !is_v_codepoint(parameter_number(fields.next()))
    {
        return ShortcutSequence::Other;
    }
    ShortcutSequence::Press
}

fn parameter_number(parameter: Option<&[u8]>) -> Option<u16> {
    let digits = parameter?.split(|byte| *byte == b':').next()?;
    std::str::from_utf8(digits).ok()?.parse().ok()
}

fn is_v_codepoint(codepoint: Option<u16>) -> bool {
    matches!(codepoint, Some(value) if value == u16::from(b'v') || value == u16::from(b'V'))
}

fn is_clipboard_modifier(encoded_modifiers: Option<u16>) -> bool {
    const ALT: u16 = 2;
    const CTRL: u16 = 4;
    const CAPS_LOCK: u16 = 64;
    const NUM_LOCK: u16 = 128;

    let Some(modifiers) = encoded_modifiers.and_then(|value| value.checked_sub(1)) else {
        return false;
    };
    let shortcut = modifiers & (ALT | CTRL);
    matches!(shortcut, ALT | CTRL) && modifiers & !(ALT | CTRL | CAPS_LOCK | NUM_LOCK) == 0
}

fn clipboard_loop(
    writer: SharedWriter,
    target: Target,
    done: Arc<AtomicBool>,
    bracketed_paste: Arc<AtomicBool>,
    receiver: mpsc::Receiver<ClipboardRequest>,
) {
    let mut transfer = None;
    while let Ok(request) = receiver.recv() {
        match request {
            ClipboardRequest::Warmup if transfer.is_none() => {
                transfer = Some(
                    transfer::TransferSession::start(&target).map_err(|error| error.to_string()),
                );
            }
            ClipboardRequest::Warmup => {}
            ClipboardRequest::Paste if !done.load(Ordering::Relaxed) => {
                paste_clipboard(&writer, &target, &mut transfer, &bracketed_paste)
            }
            ClipboardRequest::Paste => {}
            ClipboardRequest::Shutdown => return,
        }
    }
}

fn paste_clipboard(
    writer: &SharedWriter,
    target: &Target,
    transfer: &mut Option<std::result::Result<transfer::TransferSession, String>>,
    bracketed_paste: &AtomicBool,
) {
    match clipboard::read() {
        Ok(ClipboardContent::Text(text)) => {
            write_paste(writer, text.as_bytes(), bracketed_paste);
        }
        Ok(ClipboardContent::Image(image)) => match transfer.get_or_insert_with(|| {
            transfer::TransferSession::start(target).map_err(|error| error.to_string())
        }) {
            Err(error) => terminal_notice(&format!("image paste failed: {error}")),
            Ok(transfer) => match transfer.upload(&image) {
                Ok(artifact) => {
                    let marker = format!("[image: {}]", artifact.path);
                    write_paste(writer, marker.as_bytes(), bracketed_paste);
                }
                Err(error) => terminal_notice(&format!("image paste failed: {error}")),
            },
        },
        Err(error) => terminal_notice(&format!("paste failed: {error}")),
    }
}

fn write_paste(writer: &SharedWriter, bytes: &[u8], bracketed_paste: &AtomicBool) {
    let mut writer = writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = write_paste_bytes(
        &mut **writer,
        bytes,
        bracketed_paste.load(Ordering::Relaxed),
    );
}

fn write_paste_bytes<W: Write + ?Sized>(
    writer: &mut W,
    bytes: &[u8],
    bracketed_paste: bool,
) -> io::Result<()> {
    if bracketed_paste {
        writer.write_all(BRACKETED_PASTE_START)?;
    }
    writer.write_all(bytes)?;
    if bracketed_paste {
        writer.write_all(BRACKETED_PASTE_END)?;
    }
    writer.flush()
}

fn terminal_notice(message: &str) {
    let mut stderr = io::stderr().lock();
    let _ = write!(stderr, "\r\nsbc: {message}\r\n");
    let _ = stderr.flush();
}

fn resize_loop(
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    done: Arc<AtomicBool>,
    initial: (u16, u16),
) {
    let mut last = initial;
    while !done.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(250));
        let current = size().unwrap_or(last);
        if current == last {
            continue;
        }
        let _ = master
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resize(PtySize {
                rows: current.1,
                cols: current.0,
                pixel_width: 0,
                pixel_height: 0,
            });
        last = current;
    }
}

fn install_signal_restore() {
    let _ = ctrlc::set_handler(|| {
        let _ = disable_raw_mode();
        std::process::exit(130);
    });
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct ReadProbe(Arc<AtomicBool>);

    #[derive(Default)]
    struct WriteRecorder {
        writes: Vec<Vec<u8>>,
    }

    impl Write for WriteRecorder {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes.push(buffer.to_vec());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for ReadProbe {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            self.0.store(true, Ordering::SeqCst);
            Ok(0)
        }
    }

    struct FlushCheckingReader {
        flushes: Arc<AtomicUsize>,
        sent: bool,
    }

    impl Read for FlushCheckingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.sent {
                assert_eq!(self.flushes.load(Ordering::SeqCst), 1);
                return Ok(0);
            }
            self.sent = true;
            let bytes = b"first redraw";
            buffer[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    struct FlushProbe {
        flushes: Arc<AtomicUsize>,
        writes: Vec<Vec<u8>>,
    }

    impl Write for FlushProbe {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes.push(buffer.to_vec());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn terminal_output_waits_for_the_input_relay() {
        let read_started = Arc::new(AtomicBool::new(false));
        let thread_read_started = Arc::clone(&read_started);
        let (input_ready_sender, input_ready_receiver) = mpsc::sync_channel(0);
        let (thread_started_sender, thread_started_receiver) = mpsc::sync_channel(0);

        let output_thread = thread::spawn(move || {
            thread_started_sender.send(()).unwrap();
            let bracketed_paste = AtomicBool::new(false);
            relay_output_after_input_ready(
                &mut ReadProbe(thread_read_started),
                &mut io::sink(),
                &bracketed_paste,
                input_ready_receiver,
            )
            .unwrap();
        });

        thread_started_receiver.recv().unwrap();
        assert!(!read_started.load(Ordering::SeqCst));

        input_ready_sender.send(()).unwrap();
        output_thread.join().unwrap();
        assert!(read_started.load(Ordering::SeqCst));
    }

    #[test]
    fn terminal_output_flushes_each_read() {
        let (input_ready_sender, input_ready_receiver) = mpsc::sync_channel(1);
        input_ready_sender.send(()).unwrap();
        let flushes = Arc::new(AtomicUsize::new(0));
        let mut input = FlushCheckingReader {
            flushes: Arc::clone(&flushes),
            sent: false,
        };
        let mut output = FlushProbe {
            flushes,
            writes: Vec::new(),
        };
        let bracketed_paste = AtomicBool::new(false);

        relay_output_after_input_ready(
            &mut input,
            &mut output,
            &bracketed_paste,
            input_ready_receiver,
        )
        .unwrap();

        assert_eq!(output.writes, [b"first redraw".as_slice()]);
    }

    #[test]
    fn terminal_modes_track_bracketed_paste_across_reads() {
        let bracketed_paste = AtomicBool::new(false);
        let mut modes = TerminalModes::default();

        modes.observe(b"before\x1b[?20", &bracketed_paste);
        modes.observe(b"04hafter", &bracketed_paste);
        assert!(bracketed_paste.load(Ordering::Relaxed));

        modes.observe(b"\x1b[?200", &bracketed_paste);
        modes.observe(b"4l", &bracketed_paste);
        assert!(!bracketed_paste.load(Ordering::Relaxed));
    }

    #[test]
    fn ctrl_v_byte_matches_terminal_shortcut() {
        assert_eq!(CTRL_V, b'V' & 0x1f);
    }

    #[test]
    fn enhanced_ctrl_v_is_intercepted() {
        let mut forwarded = Vec::new();
        let mut relay = InputRelay::default();

        let paste_count = relay.forward(&mut forwarded, b"\x1b[118;5u", true).unwrap();

        assert!(forwarded.is_empty());
        assert_eq!(paste_count, 1);
    }

    #[test]
    fn ctrl_v_terminal_encodings_are_intercepted() {
        for sequence in [
            b"\x16".as_slice(),
            b"\x1b[118;5u",
            b"\x1b[118:86:118;5:1u",
            b"\x1b[118;69u",
            b"\x1b[27;5;118~",
        ] {
            let mut forwarded = Vec::new();

            let paste_count = InputRelay::default()
                .forward(&mut forwarded, sequence, true)
                .unwrap();

            assert!(forwarded.is_empty(), "forwarded {sequence:?}");
            assert_eq!(paste_count, 1, "did not intercept {sequence:?}");
        }
    }

    #[test]
    fn alt_v_terminal_encodings_are_intercepted() {
        for sequence in [
            b"\x1bv".as_slice(),
            b"\x1bV",
            b"\x1b[118;3u",
            b"\x1b[86;3u",
            b"\x1b[118:86:118;3:1u",
            b"\x1b[118;67u",
            b"\x1b[27;3;118~",
            b"\x1b[27;3;86~",
        ] {
            let mut forwarded = Vec::new();

            let paste_count = InputRelay::default()
                .forward(&mut forwarded, sequence, true)
                .unwrap();

            assert!(forwarded.is_empty(), "forwarded {sequence:?}");
            assert_eq!(paste_count, 1, "did not intercept {sequence:?}");
        }
    }

    #[test]
    fn enhanced_alt_v_release_is_discarded() {
        let mut forwarded = Vec::new();

        let paste_count = InputRelay::default()
            .forward(&mut forwarded, b"\x1b[118;3:3u", true)
            .unwrap();

        assert!(forwarded.is_empty());
        assert_eq!(paste_count, 0);
    }

    #[test]
    fn other_alt_keys_pass_through() {
        let input = b"\x1bx\x1b[120;3u";
        let mut forwarded = Vec::new();

        let paste_count = InputRelay::default()
            .forward(&mut forwarded, input, true)
            .unwrap();

        assert_eq!(forwarded, input);
        assert_eq!(paste_count, 0);
    }

    #[test]
    fn enhanced_ctrl_v_release_is_discarded() {
        let mut forwarded = Vec::new();

        let paste_count = InputRelay::default()
            .forward(&mut forwarded, b"\x1b[118;5:3u", true)
            .unwrap();

        assert!(forwarded.is_empty());
        assert_eq!(paste_count, 0);
    }

    #[test]
    fn enhanced_ctrl_v_can_span_reads() {
        let mut forwarded = Vec::new();
        let mut paste_count = 0;
        let mut relay = InputRelay::default();

        paste_count += relay
            .forward(&mut forwarded, b"before\x1b[118;", true)
            .unwrap();
        paste_count += relay.forward(&mut forwarded, b"5uafter", true).unwrap();

        assert_eq!(forwarded, b"beforeafter");
        assert_eq!(paste_count, 1);
    }

    #[test]
    fn terminal_responses_do_not_become_agent_input() {
        let keyboard = b"\x1b[?0u";
        let attributes = b"\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c";
        let background = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
        let input = [keyboard.as_slice(), attributes, background].concat();
        let mut forwarded = WriteRecorder::default();
        let mut relay = InputRelay::default();

        let paste_count = relay.forward(&mut forwarded, &input, true).unwrap();

        assert!(forwarded.writes.is_empty());
        assert_eq!(paste_count, 0);
        assert!(relay.take_terminal_response());
        assert!(!relay.take_terminal_response());
    }

    #[test]
    fn captured_windows_terminal_replies_do_not_become_agent_input() {
        let mut relay = InputRelay::default();
        let mut forwarded = WriteRecorder::default();
        let reads = [
            b"\x1b[?61;4;6;7;14;21;22;23;24;28;32".as_slice(),
            b";42;52c\x1b]11;rgb:0000/00",
            b"00/0000\x1b\\",
        ];
        let mut paste_count = 0;

        for read in reads {
            paste_count += relay.forward(&mut forwarded, read, true).unwrap();
        }

        assert!(forwarded.writes.is_empty());
        assert_eq!(paste_count, 0);
        assert!(relay.take_terminal_response());
    }

    #[test]
    fn clipboard_disabled_passes_enhanced_ctrl_v_through() {
        let mut forwarded = Vec::new();

        let paste_count = InputRelay::default()
            .forward(&mut forwarded, b"\x1b[118;5u", false)
            .unwrap();

        assert_eq!(forwarded, b"\x1b[118;5u");
        assert_eq!(paste_count, 0);
    }

    #[test]
    fn clipboard_disabled_passes_alt_v_through() {
        let input = b"\x1bv\x1b[118;3u";
        let mut forwarded = Vec::new();

        let paste_count = InputRelay::default()
            .forward(&mut forwarded, input, false)
            .unwrap();

        assert_eq!(forwarded, input);
        assert_eq!(paste_count, 0);
    }

    #[test]
    fn pasted_text_is_written_unchanged() {
        let mut written = Vec::new();

        write_paste_bytes(&mut written, b"clipboard text", false).unwrap();

        assert_eq!(written, b"clipboard text");
    }

    #[test]
    fn pasted_text_uses_bracketed_protocol_when_enabled() {
        let mut written = Vec::new();

        write_paste_bytes(&mut written, b"clipboard text", true).unwrap();

        assert_eq!(written, b"\x1b[200~clipboard text\x1b[201~");
    }
}
