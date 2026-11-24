use std::{
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_soulsteads-workboard")
}

fn output(dir: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(binary())
        .args(args)
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .output()
        .unwrap()
}

#[test]
fn service_install_dry_run_renders_a_loopback_unit_without_creating_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state with spaces%${FIXTURE}.sqlite3");
    let database = database.to_str().unwrap();
    let out = output(&dir, &["--db", database, "service", "install", "--dry-run"]);
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let preview = String::from_utf8(out.stdout).unwrap();
    assert!(preview.contains("[Service]\nType=simple\n"));
    assert!(preview.contains("\"web\" \"--listen\" \"127.0.0.1:7878\"\n"));
    assert!(preview.contains("state with spaces%%$${FIXTURE}.sqlite3"));
    assert!(preview.contains("\""));
    assert!(!Path::new(database).exists());
    assert!(!dir.path().join("config").exists());
}

#[test]
fn service_install_writes_a_new_user_unit_without_touching_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("board.sqlite3");
    let database = database.to_str().unwrap();
    let first = output(&dir, &["--db", database, "service", "install", "--user"]);
    assert!(
        first.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(!Path::new(database).exists());
    let unit = dir
        .path()
        .join("config/systemd/user/soulsteads-workboard.service");
    let bytes = std::fs::read(&unit).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("ExecStart="));
    assert!(
        !std::fs::symlink_metadata(&unit)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let second = output(&dir, &["--db", database, "service", "install", "--user"]);
    assert!(!second.status.success());
    assert_eq!(std::fs::read(&unit).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn service_install_refuses_an_existing_unit_symlink() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("board.sqlite3");
    let config = dir.path().join("config/systemd/user");
    std::fs::create_dir_all(&config).unwrap();
    let sentinel = dir.path().join("sentinel");
    std::fs::write(&sentinel, b"do not replace").unwrap();
    symlink(&sentinel, config.join("soulsteads-workboard.service")).unwrap();

    let out = output(
        &dir,
        &[
            "--db",
            database.to_str().unwrap(),
            "service",
            "install",
            "--user",
        ],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("refusing to overwrite"));
    assert_eq!(std::fs::read(sentinel).unwrap(), b"do not replace");
}

#[test]
fn service_install_rejects_relative_databases_and_unsafe_system_requests() {
    let dir = tempfile::tempdir().unwrap();
    let relative = output(
        &dir,
        &["--db", "board.sqlite3", "service", "install", "--dry-run"],
    );
    assert!(!relative.status.success());
    assert!(String::from_utf8_lossy(&relative.stdout).contains("absolute"));

    let database: PathBuf = dir.path().join("board.sqlite3");
    let database = database.to_str().unwrap();
    let missing_account = output(
        &dir,
        &[
            "--db",
            database,
            "service",
            "install",
            "--system",
            "--dry-run",
        ],
    );
    assert!(!missing_account.status.success());
    assert!(String::from_utf8_lossy(&missing_account.stdout).contains("--run-as"));

    let root_account = output(
        &dir,
        &[
            "--db",
            database,
            "service",
            "install",
            "--system",
            "--run-as",
            "root",
            "--dry-run",
        ],
    );
    assert!(!root_account.status.success());
    assert!(String::from_utf8_lossy(&root_account.stdout).contains("non-root"));
}

#[test]
fn version_uses_the_shipped_executable_name() {
    let dir = tempfile::tempdir().unwrap();
    let result = output(&dir, &["--version"]);
    assert!(result.status.success());
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .starts_with("soulsteads-workboard ")
    );
}
