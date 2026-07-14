use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn rewind(binary: &Path, home: &Path, current_dir: &Path, arguments: &[&str]) -> Output {
    Command::new(binary)
        .args(arguments)
        .env("REWIND_HOME", home)
        .env("NO_COLOR", "1")
        .current_dir(current_dir)
        .output()
        .unwrap()
}

fn last_stdout_line(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .next_back()
        .unwrap()
}

#[test]
fn public_cli_records_replays_checks_out_forks_and_compares() {
    let temporary = TempDir::new().unwrap();
    let source = temporary.path().join("source");
    let home = temporary.path().join("store");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("state.txt"), b"original\n").unwrap();

    let binary = Path::new(env!("CARGO_BIN_EXE_rewind"));
    let source_argument = source.to_str().unwrap();
    let recorded = rewind(
        binary,
        &home,
        temporary.path(),
        &[
            "run",
            "--workspace",
            source_argument,
            "--id-only",
            "--",
            "sh",
            "-c",
            "printf 'integration\\n'; printf 'changed\\n' > state.txt; exit 7",
        ],
    );
    assert_eq!(recorded.status.code(), Some(7), "{recorded:?}");
    let original_run = last_stdout_line(&recorded).to_owned();
    assert_eq!(
        std::fs::read(source.join("state.txt")).unwrap(),
        b"original\n"
    );

    let replayed = rewind(binary, &home, temporary.path(), &["replay", &original_run]);
    assert!(replayed.status.success(), "{replayed:?}");
    assert_eq!(replayed.stdout, b"integration\r\n");

    let checkout = temporary.path().join("checkout");
    let selector = format!("{original_run}@final");
    let checked_out = rewind(
        binary,
        &home,
        temporary.path(),
        &["checkout", &selector, "--to", checkout.to_str().unwrap()],
    );
    assert!(checked_out.status.success(), "{checked_out:?}");
    assert_eq!(
        std::fs::read(checkout.join("state.txt")).unwrap(),
        b"changed\n"
    );

    let fork_selector = format!("{original_run}@initial");
    let forked = rewind(
        binary,
        &home,
        temporary.path(),
        &[
            "fork",
            &fork_selector,
            "--id-only",
            "--",
            "sh",
            "-c",
            "printf 'forked\\n' > state.txt",
        ],
    );
    assert!(forked.status.success(), "{forked:?}");
    let forked_run = last_stdout_line(&forked).to_owned();

    let compared = rewind(
        binary,
        &home,
        temporary.path(),
        &[
            "compare",
            &original_run,
            &forked_run,
            "--test",
            "grep -qx forked state.txt",
            "--json",
        ],
    );
    assert!(compared.status.success(), "{compared:?}");
    let comparison: Value = serde_json::from_slice(&compared.stdout).unwrap();
    assert_eq!(comparison["left"]["run_id"], original_run);
    assert_eq!(comparison["right"]["run_id"], forked_run);
    assert_eq!(comparison["relationship"]["kind"], "left_parent_of_right");
    assert_eq!(comparison["file_changes"][0]["path"], "state.txt");
    assert_eq!(comparison["evaluation"]["left"]["exit_status"]["value"], 1);
    assert_eq!(comparison["evaluation"]["right"]["exit_status"]["value"], 0);
    assert_eq!(
        std::fs::read(source.join("state.txt")).unwrap(),
        b"original\n"
    );
}
