use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use arboard::{Clipboard, ImageData};

use crate::Result;

pub(crate) enum ClipboardContent {
    Image(TemporaryImage),
    Text(String),
}

pub(crate) struct TemporaryImage {
    path: PathBuf,
    name: String,
}

impl TemporaryImage {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TemporaryImage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn read() -> Result<ClipboardContent> {
    if is_wsl() {
        return read_windows_clipboard_from_wsl();
    }
    read_native_clipboard()
}

fn read_native_clipboard() -> Result<ClipboardContent> {
    let mut clipboard =
        Clipboard::new().map_err(|error| format!("clipboard unavailable: {error}"))?;
    if let Ok(image) = clipboard.get_image() {
        return encode_image(image).map(ClipboardContent::Image);
    }
    clipboard
        .get_text()
        .map(ClipboardContent::Text)
        .map_err(|error| format!("clipboard contains no text or image: {error}").into())
}

pub(crate) fn encode_image(image: ImageData<'_>) -> Result<TemporaryImage> {
    let temporary = create_temporary_image()?;
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&temporary.path)?;
    let mut encoder = png::Encoder::new(
        BufWriter::new(file),
        u32::try_from(image.width)?,
        u32::try_from(image.height)?,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(image.bytes.as_ref())?;
    Ok(temporary)
}

fn read_windows_clipboard_from_wsl() -> Result<ClipboardContent> {
    let temporary = create_temporary_image()?;
    let image_script = concat!(
        "Add-Type -AssemblyName System.Windows.Forms; ",
        "Add-Type -AssemblyName System.Drawing; ",
        "$image = [Windows.Forms.Clipboard]::GetImage(); ",
        "if ($null -eq $image) { exit 3 }; ",
        "$image.Save($env:SBC_CLIPBOARD_PATH, [Drawing.Imaging.ImageFormat]::Png)"
    );
    let mut image_command = Command::new("powershell.exe");
    image_command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-Command",
            image_script,
        ])
        .env("SBC_CLIPBOARD_PATH", &temporary.path)
        .env("WSLENV", wslenv_with_path("SBC_CLIPBOARD_PATH"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let image_status = image_command
        .status()
        .map_err(|error| format!("could not read the Windows clipboard: {error}"))?;
    if image_status.success() {
        return Ok(ClipboardContent::Image(temporary));
    }
    drop(temporary);

    let text_script = concat!(
        "$OutputEncoding = [Console]::OutputEncoding = ",
        "[Text.UTF8Encoding]::new(); ",
        "$value = Get-Clipboard -Raw; ",
        "if ($null -eq $value) { exit 3 }; ",
        "[Console]::Out.Write($value)"
    );
    let text = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-Command",
            text_script,
        ])
        .output()
        .map_err(|error| format!("could not read the Windows clipboard: {error}"))?;
    if !text.status.success() {
        return Err("Windows clipboard contains no text or image".into());
    }
    Ok(ClipboardContent::Text(String::from_utf8(text.stdout)?))
}

fn wslenv_with_path(name: &str) -> String {
    let path_entry = format!("{name}/p");
    match std::env::var("WSLENV") {
        Ok(existing) if !existing.is_empty() => format!("{existing}:{path_entry}"),
        _ => path_entry,
    }
}

fn create_temporary_image() -> Result<TemporaryImage> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for suffix in 0..16_u8 {
        let name = format!("sbc-{}-{timestamp}-{suffix}.png", std::process::id());
        let path = std::env::temp_dir().join(&name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.flush()?;
                return Ok(TemporaryImage { path, name });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("could not allocate a temporary clipboard image".into())
}

fn is_wsl() -> bool {
    cfg!(target_os = "linux")
        && (std::env::var_os("WSL_DISTRO_NAME").is_some()
            || fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft")))
}

#[cfg(test)]
pub(crate) fn _owned_image(bytes: Vec<u8>, width: usize, height: usize) -> ImageData<'static> {
    ImageData {
        width,
        height,
        bytes: std::borrow::Cow::Owned(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_rgba_image_as_png() {
        let image = _owned_image(vec![255, 0, 0, 255], 1, 1);
        let temporary = encode_image(image).unwrap();
        let bytes = fs::read(temporary.path()).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn temporary_image_removes_itself() {
        let path = {
            let temporary = create_temporary_image().unwrap();
            temporary.path().to_owned()
        };
        assert!(!path.exists());
    }

    #[test]
    fn appends_wsl_path_translation() {
        // The environment itself is process-global, so exercise the empty/default
        // contract here and leave inherited WSLENV coverage to the integration path.
        let value = wslenv_with_path("SBC_CLIPBOARD_PATH");
        assert!(
            value
                .split(':')
                .any(|entry| entry == "SBC_CLIPBOARD_PATH/p")
        );
    }
}
