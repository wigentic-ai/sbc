use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::clipboard::TemporaryImage;
use crate::command::quote;
use crate::target::{Location, Target};

const SANDBOX_DIRECTORY: &str = "/tmp/sbc";

#[derive(Clone, Debug)]
pub(crate) struct Artifact {
    pub(crate) path: String,
}

pub(crate) struct TransferSession {
    directory: String,
    child: Option<Child>,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl TransferSession {
    pub(crate) fn start(target: &Target) -> Result<Self> {
        let directory = session_directory()?;
        let script = receiver_script(&directory);
        let mut command = match &target.location {
            Location::Local { sbx_command } => {
                let mut command = Command::new(sbx_command);
                command.args(["exec", "-i", &target.sandbox, "sh", "-c", &script]);
                command
            }
            Location::Remote {
                ssh_host,
                sbx_command,
            } => {
                let remote = format!(
                    "{} exec -i {} sh -c {}",
                    quote(sbx_command),
                    quote(&target.sandbox),
                    quote(&script)
                );
                let mut command = Command::new("ssh");
                command
                    .args(["-o", "BatchMode=yes"])
                    .arg(ssh_host)
                    .arg(remote);
                command
            }
        };
        Self::start_command(&mut command, directory)
    }

    fn start_command(command: &mut Command, directory: String) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start image transfer: {error}"))?;
        let input = child
            .stdin
            .take()
            .ok_or("image transfer stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("image transfer stdout unavailable")?;
        let mut output = BufReader::new(stdout);
        let mut ready = String::new();
        if output.read_line(&mut ready)? == 0 || ready != "ready\n" {
            drop(input);
            let _ = child.wait();
            return Err("image transfer helper did not become ready".into());
        }
        Ok(Self {
            directory,
            child: Some(child),
            input: Some(input),
            output,
        })
    }

    pub(crate) fn upload(&mut self, image: &TemporaryImage) -> Result<Artifact> {
        let size = image.path().metadata()?.len();
        let input = self
            .input
            .as_mut()
            .ok_or("image transfer helper is closed")?;
        writeln!(input, "put {} {size}", image.name())?;
        io::copy(&mut File::open(image.path())?, input)?;
        input.flush()?;

        let mut acknowledgement = String::new();
        if self.output.read_line(&mut acknowledgement)? == 0 || acknowledgement != "ok\n" {
            return Err("image transfer helper did not acknowledge upload".into());
        }
        Ok(Artifact {
            path: format!("{}/{}", self.directory, image.name()),
        })
    }
}

impl Drop for TransferSession {
    fn drop(&mut self) {
        drop(self.input.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

fn session_directory() -> Result<String> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!(
        "{SANDBOX_DIRECTORY}/{}-{timestamp}",
        std::process::id()
    ))
}

fn receiver_script(directory: &str) -> String {
    format!(
        concat!(
            "set -eu; directory={}; mkdir -p \"$directory\"; ",
            "cleanup() {{ for file in \"$directory\"/*; do ",
            "[ ! -e \"$file\" ] || unlink \"$file\"; done; ",
            "rmdir \"$directory\" 2>/dev/null || true; }}; ",
            "trap cleanup EXIT HUP INT TERM; printf 'ready\\n'; ",
            "while IFS=' ' read -r action name size; do ",
            "[ \"$action\" = put ] || exit 4; ",
            "case \"$name\" in ''|*[!A-Za-z0-9._-]*) exit 5;; esac; ",
            "head -c \"$size\" > \"$directory/$name\"; printf 'ok\\n'; done"
        ),
        quote(directory)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_artifacts_stay_in_tmp() {
        let image = crate::clipboard::_owned_image(vec![0, 0, 0, 255], 1, 1);
        let temporary = crate::clipboard::encode_image(image).unwrap();
        let directory = session_directory().unwrap();
        let path = format!("{directory}/{}", temporary.name());
        assert!(path.starts_with("/tmp/sbc/"));
        assert!(path.rsplit('/').next().unwrap().starts_with("sbc-"));
        assert!(path.ends_with(".png"));
    }

    #[test]
    fn reuses_one_transfer_process_and_cleans_its_directory() {
        let directory = session_directory().unwrap();
        let script = receiver_script(&directory);
        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        let mut transfer = TransferSession::start_command(&mut command, directory.clone()).unwrap();
        let first = crate::clipboard::encode_image(crate::clipboard::_owned_image(
            vec![255, 0, 0, 255],
            1,
            1,
        ))
        .unwrap();
        let second = crate::clipboard::encode_image(crate::clipboard::_owned_image(
            vec![0, 0, 255, 255],
            1,
            1,
        ))
        .unwrap();

        let first_artifact = transfer.upload(&first).unwrap();
        let second_artifact = transfer.upload(&second).unwrap();

        assert_eq!(
            std::fs::read(&first_artifact.path).unwrap(),
            std::fs::read(first.path()).unwrap()
        );
        assert_eq!(
            std::fs::read(&second_artifact.path).unwrap(),
            std::fs::read(second.path()).unwrap()
        );
        drop(transfer);
        assert!(!std::path::Path::new(&directory).exists());
    }
}
