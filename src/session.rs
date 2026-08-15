use std::io::{self, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    let writer = pair.master.take_writer()?;
    let master = Arc::new(Mutex::new(pair.master));
    let done = Arc::new(AtomicBool::new(false));
    let artifacts = Arc::new(Mutex::new(Vec::<Artifact>::new()));

    let raw_mode = RawModeGuard::enable()?;
    install_signal_restore();

    let output_thread = thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        let _ = io::copy(&mut reader, &mut stdout);
        let _ = stdout.flush();
    });

    let input_target = target.clone();
    let input_artifacts = Arc::clone(&artifacts);
    thread::spawn(move || input_loop(writer, input_target, input_artifacts, clipboard_enabled));

    let resize_master = Arc::clone(&master);
    let resize_done = Arc::clone(&done);
    thread::spawn(move || resize_loop(resize_master, resize_done, (columns, rows)));

    let status = child.wait()?;
    done.store(true, Ordering::Relaxed);
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

fn input_loop(
    mut writer: Box<dyn Write + Send>,
    target: Target,
    artifacts: Arc<Mutex<Vec<Artifact>>>,
    clipboard_enabled: bool,
) {
    let mut stdin = io::stdin().lock();
    let mut buffer = [0_u8; 1024];
    loop {
        let count = match stdin.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        for segment in buffer[..count].split_inclusive(|byte| *byte == CTRL_V) {
            let intercepted = clipboard_enabled && segment.last() == Some(&CTRL_V);
            let plain = if intercepted {
                &segment[..segment.len() - 1]
            } else {
                segment
            };
            if writer.write_all(plain).is_err() {
                return;
            }
            if intercepted {
                paste_clipboard(&mut writer, &target, &artifacts);
            }
        }
        if writer.flush().is_err() {
            return;
        }
    }
}

fn paste_clipboard(
    writer: &mut Box<dyn Write + Send>,
    target: &Target,
    artifacts: &Arc<Mutex<Vec<Artifact>>>,
) {
    match clipboard::read() {
        Ok(ClipboardContent::Text(text)) => {
            let _ = writer.write_all(text.as_bytes());
        }
        Ok(ClipboardContent::Image(image)) => match transfer::upload(target, &image) {
            Ok(artifact) => {
                let marker = format!("[image: {}]", artifact.path);
                let _ = writer.write_all(marker.as_bytes());
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

    #[test]
    fn ctrl_v_byte_matches_terminal_shortcut() {
        assert_eq!(CTRL_V, b'V' & 0x1f);
    }
}
