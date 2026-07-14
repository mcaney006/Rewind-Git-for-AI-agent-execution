use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut arguments = env::args_os().skip(1);
    let command = arguments.next().ok_or_else(usage)?;
    if arguments.next().is_some() {
        return Err(usage());
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "xtask manifest has no workspace parent".to_owned())?;
    match command.to_str() {
        Some("check") => fast_check(workspace),
        Some("test") => cargo(workspace, ["test", "--workspace"]),
        Some("test-all") => test_all(workspace),
        Some("lint") => lint(workspace),
        Some("coverage") => coverage(workspace),
        Some("bench") => cargo(
            workspace,
            [
                "bench",
                "-p",
                "rewind-benchmarks",
                "--features",
                "run-benchmarks",
            ],
        ),
        Some("demo") => demo(workspace),
        Some("doctor") => cargo(
            workspace,
            ["run", "--quiet", "--bin", "rewind", "--", "doctor"],
        ),
        Some("package") => package(workspace),
        _ => Err(usage()),
    }
}

fn fast_check(workspace: &Path) -> Result<(), String> {
    cargo(workspace, ["fmt", "--all", "--check"])?;
    cargo(workspace, ["check", "--workspace", "--all-targets"])?;
    clippy(workspace)?;
    cargo(workspace, ["test", "--workspace", "--lib"])
}

fn test_all(workspace: &Path) -> Result<(), String> {
    cargo(workspace, ["test", "--workspace"])?;
    cargo(workspace, ["test", "--workspace", "--doc"])
}

fn lint(workspace: &Path) -> Result<(), String> {
    cargo(workspace, ["fmt", "--all", "--check"])?;
    clippy(workspace)
}

fn clippy(workspace: &Path) -> Result<(), String> {
    cargo(
        workspace,
        [
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )
}

fn coverage(workspace: &Path) -> Result<(), String> {
    if !cargo_plugin_available("llvm-cov") {
        return Err(
            "cargo-llvm-cov is not installed; install it explicitly before requesting coverage"
                .to_owned(),
        );
    }
    cargo(workspace, ["llvm-cov", "--workspace", "--all-targets"])
}

fn package(workspace: &Path) -> Result<(), String> {
    cargo(
        workspace,
        ["build", "--release", "--locked", "--bin", "rewind"],
    )?;
    cargo(
        workspace,
        [
            "run",
            "--quiet",
            "--bin",
            "rewind",
            "--",
            "completions",
            "--shell",
            "all",
            "--output",
            "target/completions",
        ],
    )?;
    cargo(
        workspace,
        [
            "run",
            "--quiet",
            "--bin",
            "rewind",
            "--",
            "man",
            "--output",
            "target/man/rewind.1",
        ],
    )
}

fn demo(workspace: &Path) -> Result<(), String> {
    let fixture = workspace.join("fixtures/repositories/demo");
    let breaker = workspace.join("fixtures/agents/breaker.sh");
    let fixer = workspace.join("fixtures/agents/fixer.sh");
    for required in [&fixture, &breaker, &fixer] {
        if !required.exists() {
            return Err(format!("demo fixture is missing: {}", required.display()));
        }
    }
    cargo(workspace, ["build", "--quiet", "--bin", "rewind"])?;
    let demo_root = env::var_os("REWIND_DEMO_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target/rewind-demo"));
    if demo_root.exists() {
        std::fs::remove_dir_all(&demo_root)
            .map_err(|error| format!("remove old demo {}: {error}", demo_root.display()))?;
    }
    std::fs::create_dir_all(&demo_root)
        .map_err(|error| format!("create demo {}: {error}", demo_root.display()))?;
    let source = demo_root.join("source");
    copy_fixture(&fixture, &source)?;
    let home = demo_root.join("home");
    let rewind = workspace.join("target/debug/rewind");
    let breaker_output = invoke(
        Command::new(&rewind)
            .env("REWIND_HOME", &home)
            .arg("run")
            .arg("--workspace")
            .arg(&source)
            .arg("--id-only")
            .arg("--")
            .arg(&breaker),
        "record breaker agent",
        false,
    )?;
    let breaker_run = last_nonempty_line(&breaker_output)
        .ok_or_else(|| "breaker run did not print a machine-readable run ID".to_owned())?;
    let checkpoint = invoke(
        Command::new(&rewind).env("REWIND_HOME", &home).args([
            "show",
            breaker_run,
            "--checkpoint",
            "before-bad-change",
            "--id-only",
        ]),
        "resolve demo checkpoint",
        true,
    )?;
    let checkpoint = last_nonempty_line(&checkpoint)
        .ok_or_else(|| "demo checkpoint lookup returned no ID".to_owned())?;
    let selector = format!("{breaker_run}@{checkpoint}");
    let fork_output = invoke(
        Command::new(&rewind)
            .env("REWIND_HOME", &home)
            .arg("fork")
            .arg(&selector)
            .arg("--id-only")
            .arg("--")
            .arg(&fixer),
        "record fixer fork",
        true,
    )?;
    let fixer_run = last_nonempty_line(&fork_output)
        .ok_or_else(|| "fixer fork did not print a run ID".to_owned())?;
    invoke(
        Command::new(&rewind).env("REWIND_HOME", &home).args([
            "compare",
            breaker_run,
            fixer_run,
            "--test",
            "./test.sh",
        ]),
        "compare demo runs",
        true,
    )?;
    println!("Demo data  {}", demo_root.display());
    println!(
        "Replay     REWIND_HOME={} {} replay {breaker_run}",
        home.display(),
        rewind.display()
    );
    println!(
        "Fork replay REWIND_HOME={} {} replay {fixer_run}",
        home.display(),
        rewind.display()
    );
    Ok(())
}

fn cargo<const N: usize>(workspace: &Path, arguments: [&str; N]) -> Result<(), String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(cargo);
    command.current_dir(workspace).args(arguments);
    invoke(&mut command, "run Cargo", true).map(|_| ())
}

fn cargo_plugin_available(name: &str) -> bool {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    Command::new(cargo)
        .args([name, "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn invoke(
    command: &mut Command,
    description: &str,
    require_success: bool,
) -> Result<String, String> {
    eprintln!("+ {command:?}");
    let output = command
        .output()
        .map_err(|error| format!("{description}: {error}"))?;
    if !output.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    if require_success && !output.status.success() {
        return Err(format!("{description} exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn copy_fixture(source: &Path, destination: &Path) -> Result<(), String> {
    std::fs::create_dir(destination).map_err(|error| {
        format!(
            "create fixture destination {}: {error}",
            destination.display()
        )
    })?;
    for entry in std::fs::read_dir(source)
        .map_err(|error| format!("read fixture {}: {error}", source.display()))?
    {
        let entry = entry.map_err(|error| format!("read fixture entry: {error}"))?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if entry
            .file_type()
            .map_err(|error| format!("inspect {}: {error}", from.display()))?
            .is_dir()
        {
            copy_fixture(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|error| format!("copy {} to {}: {error}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn last_nonempty_line(value: &str) -> Option<&str> {
    value
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
}

fn usage() -> String {
    "usage: cargo xtask <check|test|test-all|lint|coverage|bench|demo|doctor|package>".to_owned()
}
