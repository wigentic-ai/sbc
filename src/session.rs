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
use crate::transfer::Artifact;
use crate::{Result, clipboard, transfer};

const CTRL_V: u8 = 0x16;
const ESCAPE: u8 = 0x1b;
const MAX_CSI_SEQUENCE: usize = 64;

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

enum ClipboardRequest {
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
    let artifacts = Arc::new(Mutex::new(Vec::<Artifact>::new()));

    let raw_mode = RawModeGuard::enable()?;
    install_signal_restore();

    // Clipboard reads and sandbox uploads can take seconds. Keep them off the
    // input relay so terminal capability replies and keystrokes keep flowing.
    let (clipboard_sender, clipboard_receiver) = mpsc::channel();
    let clipboard_writer = Arc::clone(&writer);
    let clipboard_target = target.clone();
    let clipboard_artifacts = Arc::clone(&artifacts);
    let clipboard_done = Arc::clone(&done);
    let clipboard_thread = thread::spawn(move || {
        clipboard_loop(
            clipboard_writer,
            clipboard_target,
            clipboard_artifacts,
            clipboard_done,
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

    let output_thread = thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        let _ = relay_output_after_input_ready(&mut reader, &mut stdout, input_ready_receiver);
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

    let artifacts = artifacts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    transfer::cleanup(&target, &artifacts);

    if !status.success() {
        return Err(format!("session {status}").into());
    }
    Ok(())
}

fn relay_output_after_input_ready<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    input_ready: mpsc::Receiver<()>,
) -> io::Result<()> {
    // Some TUIs query terminal capabilities during their first few milliseconds
    // and use a short response window. Do not expose those queries to the outer
    // terminal until the return path is already listening.
    input_ready
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "input relay stopped"))?;
    io::copy(reader, writer)?;
    writer.flush()
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
            if bytes[cursor..].starts_with(b"\x1bv") {
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

            if bytes[cursor] != ESCAPE || bytes.get(cursor + 1) != Some(&b'[') {
                cursor += 1;
                continue;
            }

            let Some(sequence_end) = csi_sequence_end(&bytes[cursor..]) else {
                if bytes.len() - cursor > MAX_CSI_SEQUENCE {
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
                ShortcutSequence::Other => {}
            }
            cursor = sequence_end;
        }

        writer.write_all(&bytes[plain_start..])?;
        Ok(paste_count)
    }
}

fn csi_sequence_end(input: &[u8]) -> Option<usize> {
    debug_assert!(input.starts_with(b"\x1b["));
    input[2..]
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|index| index + 3)
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
    if parameter_number(fields.next()) != Some(u16::from(b'v')) {
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
        || parameter_number(fields.next()) != Some(u16::from(b'v'))
    {
        return ShortcutSequence::Other;
    }
    ShortcutSequence::Press
}

fn parameter_number(parameter: Option<&[u8]>) -> Option<u16> {
    let digits = parameter?.split(|byte| *byte == b':').next()?;
    std::str::from_utf8(digits).ok()?.parse().ok()
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
    artifacts: Arc<Mutex<Vec<Artifact>>>,
    done: Arc<AtomicBool>,
    receiver: mpsc::Receiver<ClipboardRequest>,
) {
    while let Ok(request) = receiver.recv() {
        match request {
            ClipboardRequest::Paste if !done.load(Ordering::Relaxed) => {
                paste_clipboard(&writer, &target, &artifacts)
            }
            ClipboardRequest::Paste => {}
            ClipboardRequest::Shutdown => return,
        }
    }
}

fn paste_clipboard(writer: &SharedWriter, target: &Target, artifacts: &Arc<Mutex<Vec<Artifact>>>) {
    match clipboard::read() {
        Ok(ClipboardContent::Text(text)) => {
            write_paste(writer, text.as_bytes());
        }
        Ok(ClipboardContent::Image(image)) => match transfer::upload(target, &image) {
            Ok(artifact) => {
                let marker = format!("[image: {}]", artifact.path);
                write_paste(writer, marker.as_bytes());
                artifacts
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(artifact);
            }
            Err(error) => terminal_notice(&format!("image paste failed: {error}")),
        },
        Err(error) => terminal_notice(&format!("paste failed: {error}")),
    }
}

fn write_paste(writer: &SharedWriter, bytes: &[u8]) {
    let mut writer = writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = writer.write_all(bytes);
    let _ = writer.flush();
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

    struct ReadProbe(Arc<AtomicBool>);

    impl Read for ReadProbe {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            self.0.store(true, Ordering::SeqCst);
            Ok(0)
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
            relay_output_after_input_ready(
                &mut ReadProbe(thread_read_started),
                &mut io::sink(),
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
            b"\x1b[118;3u",
            b"\x1b[118:86:118;3:1u",
            b"\x1b[118;67u",
            b"\x1b[27;3;118~",
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
    fn terminal_responses_follow_ctrl_v_without_being_delayed() {
        let terminal_responses = concat!(
            "\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c",
            "\x1b]11;rgb:0000/0000/0000\x1b\\"
        );
        let input = format!("\x16{terminal_responses}");
        let mut forwarded = Vec::new();

        let paste_count = InputRelay::default()
            .forward(&mut forwarded, input.as_bytes(), true)
            .unwrap();

        assert_eq!(forwarded, terminal_responses.as_bytes());
        assert_eq!(paste_count, 1);
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
}
