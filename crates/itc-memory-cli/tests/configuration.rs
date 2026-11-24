//! Configuration discovery through the installed command-line entry point.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("itc-memory-config-{}-{nonce}", std::process::id()));
        fs::create_dir_all(path.join("home/.config/itc-memory")).expect("create home");
        fs::create_dir_all(path.join("work")).expect("create work directory");
        Self(path)
    }

    fn run(&self) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_itc-memory"))
            .current_dir(self.0.join("work"))
            .env("HOME", self.0.join("home"))
            .arg("--help")
            .output()
            .expect("run CLI")
    }

    fn write(&self, path: impl AsRef<Path>, content: &str) {
        fs::write(self.0.join(path), content).expect("write fixture");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove fixture");
    }
}

#[test]
fn user_configuration_takes_precedence_over_unrelated_working_directory() {
    let fixture = Fixture::new();
    fixture.write("home/.config/itc-memory/.env", "CONFIG_TEST=value\n");
    fixture.write("work/.env", "not a valid assignment\n");
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn invalid_user_configuration_is_reported_instead_of_skipped() {
    let fixture = Fixture::new();
    fixture.write("home/.config/itc-memory/.env", "not a valid assignment\n");
    fixture.write("work/.env", "CONFIG_TEST=value\n");
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("load environment configuration"));
}

#[cfg(unix)]
#[test]
fn launcher_prefers_the_installed_mcp_binary() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let bin = fixture.0.join("home/.cargo/bin");
    fs::create_dir_all(&bin).expect("create binary directory");
    let mcp = bin.join("itc-memory-mcp");
    fs::write(&mcp, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").expect("write fake MCP");
    fs::set_permissions(&mcp, fs::Permissions::from_mode(0o755)).expect("make fake MCP executable");

    let launcher = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/itc-memory-mcp.sh");
    let output = Command::new("bash")
        .arg(launcher)
        .arg("--help")
        .env("HOME", fixture.0.join("home"))
        .output()
        .expect("run launcher");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "--transport\nstdio\n--help\n"
    );
}
