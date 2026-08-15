use std::fs::File;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::Result;
use crate::clipboard::TemporaryImage;
use crate::command::quote;
use crate::target::{Location, Target};

const SANDBOX_DIRECTORY: &str = "/tmp/sbc";

#[derive(Clone, Debug)]
pub(crate) struct Artifact {
    pub(crate) path: String,
}

pub(crate) fn upload(target: &Target, image: &TemporaryImage) -> Result<Artifact> {
    let sandbox_path = format!("{SANDBOX_DIRECTORY}/{}", image.name());
    match &target.location {
        Location::Local { sbx_command } => {
            let script = write_script(&sandbox_path);
            let mut command = Command::new(sbx_command);
            command.args(["exec", "-i", &target.sandbox, "sh", "-c", &script]);
            stream_file(&mut command, image.path(), "copy image into sandbox")?;
        }
        Location::Remote {
            ssh_host,
            sbx_command,
        } => upload_remote(
            ssh_host,
            sbx_command,
            &target.sandbox,
            image.path(),
            &sandbox_path,
        )?,
    }
    Ok(Artifact { path: sandbox_path })
}

pub(crate) fn cleanup(target: &Target, artifacts: &[Artifact]) {
    for artifact in artifacts {
        let status = match &target.location {
            Location::Local { sbx_command } => Command::new(sbx_command)
                .args(["exec", &target.sandbox, "rm", "-f", &artifact.path])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
            Location::Remote {
                ssh_host,
                sbx_command,
            } => {
                let remote = format!(
                    "{} exec {} rm -f {}",
                    quote(sbx_command),
                    quote(&target.sandbox),
                    quote(&artifact.path)
                );
                Command::new("ssh")
                    .args(["-o", "BatchMode=yes"])
                    .arg(ssh_host)
                    .arg(remote)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
            }
        };
        let _ = status;
    }
}

fn upload_remote(
    ssh_host: &str,
    sbx: &str,
    sandbox: &str,
    local_path: &Path,
    sandbox_path: &str,
) -> Result<()> {
    let script = write_script(sandbox_path);
    let remote = format!(
        "{} exec -i {} sh -c {}",
        quote(sbx),
        quote(sandbox),
        quote(&script)
    );
    let mut command = Command::new("ssh");
    command
        .args(["-o", "BatchMode=yes"])
        .arg(ssh_host)
        .arg(remote);
    stream_file(&mut command, local_path, "copy image into remote sandbox")
}

fn write_script(sandbox_path: &str) -> String {
    format!(
        "set -eu; mkdir -p {}; cat > {}",
        quote(SANDBOX_DIRECTORY),
        quote(sandbox_path)
    )
}

fn stream_file(command: &mut Command, local_path: &Path, action: &str) -> Result<()> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not {action}: {error}"))?;
    let mut source = File::open(local_path)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("image transfer stdin unavailable")?;
    io::copy(&mut source, &mut stdin)?;
    drop(stdin);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "could not {action}: command exited with {}{}",
            output.status,
            stderr_detail(&detail)
        )
        .into());
    }
    Ok(())
}

fn stderr_detail(stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_artifacts_stay_in_tmp() {
        let image = crate::clipboard::_owned_image(vec![0, 0, 0, 255], 1, 1);
        let temporary = crate::clipboard::encode_image(image).unwrap();
        let path = format!("{SANDBOX_DIRECTORY}/{}", temporary.name());
        assert!(path.starts_with("/tmp/sbc/sbc-"));
        assert!(path.ends_with(".png"));
    }
}
