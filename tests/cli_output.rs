use std::process::{Command, Output};

fn run(argument: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sbc"))
        .arg(argument)
        .output()
        .expect("sbc should start")
}

#[test]
fn help_is_successful_output() {
    let output = run("--help");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("Connect to Docker Sandboxes"));
    assert!(output.stderr.is_empty());
}

#[test]
fn version_is_successful_output() {
    let output = run("--version");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "sbc 0.1.2\n");
    assert!(output.stderr.is_empty());
}
