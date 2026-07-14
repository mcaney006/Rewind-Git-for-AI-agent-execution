use std::env;
use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rewind_domain::{
    BranchId, CapturePolicy, Checkpoint, CheckpointId, CheckpointReason, Event, EventPayload,
    EventSequence, MonotonicDuration, ObjectId, Platform, ProcessExitStatus, Run, RunId, RunStatus,
    Snapshot, SnapshotId, TerminalStreamId, Timestamp,
};
use rewind_snapshot::{
    MaterializeOptions, ScanOptions, diff_snapshots, materialize, scan_workspace,
};
use rewind_store::{RunFinish, Store};
use tempfile::TempDir;

const DEFAULT_SAMPLES: usize = 3;
const DEFAULT_FILES: usize = 2_000;
const DEFAULT_CHANGED_FILES: usize = 32;
const DEFAULT_EVENTS: usize = 10_000;
const EVENT_BATCH: usize = 256;
const TIMELINE_PAGE: u32 = 256;

type BenchResult<T = ()> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy)]
struct Workload {
    samples: usize,
    files: usize,
    changed_files: usize,
    events: usize,
}

fn main() -> BenchResult {
    let workload = Workload::from_environment()?;
    println!(
        "Rewind benchmark: {}-{}, samples={}, files={}, changed={}, events={}",
        env::consts::OS,
        env::consts::ARCH,
        workload.samples,
        workload.files,
        workload.changed_files,
        workload.events
    );

    report("initial_snapshot", benchmark_initial_snapshot(workload)?);
    report(
        "changed_subset_and_dedup",
        benchmark_changed_subset(workload)?,
    );
    report("checkpoint_commit", benchmark_checkpoint_commit(workload)?);
    report("materialization", benchmark_materialization(workload)?);
    report("run_comparison", benchmark_run_comparison(workload)?);
    let (ingestion, query) = benchmark_timeline(workload)?;
    report("terminal_frame_ingestion", ingestion);
    report("timeline_query", query);
    Ok(())
}

impl Workload {
    fn from_environment() -> BenchResult<Self> {
        let samples = setting("REWIND_BENCH_SAMPLES", DEFAULT_SAMPLES)?;
        let files = setting("REWIND_BENCH_FILES", DEFAULT_FILES)?;
        let changed_files = setting("REWIND_BENCH_CHANGED_FILES", DEFAULT_CHANGED_FILES)?;
        let events = setting("REWIND_BENCH_EVENTS", DEFAULT_EVENTS)?;
        if changed_files > files {
            return Err("REWIND_BENCH_CHANGED_FILES cannot exceed REWIND_BENCH_FILES".into());
        }
        Ok(Self {
            samples,
            files,
            changed_files,
            events,
        })
    }
}

fn setting(name: &str, default: usize) -> BenchResult<usize> {
    match env::var(name) {
        Ok(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("{name} must be a positive integer").into()),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(source) => Err(format!("cannot read {name}: {source}").into()),
    }
}

fn benchmark_initial_snapshot(workload: Workload) -> BenchResult<Vec<Duration>> {
    let fixture = TempDir::new()?;
    let source = fixture.path().join("source");
    create_fixture(&source, workload.files)?;
    let mut samples = Vec::with_capacity(workload.samples);
    for sample in 0..workload.samples {
        let store_root = fixture.path().join(format!("initial-store-{sample}"));
        let mut store = Store::open(store_root)?;
        let started = Instant::now();
        let report = scan_workspace(&source, &mut store, &ScanOptions::default(), 0)?;
        samples.push(started.elapsed());
        black_box(report.snapshot.id);
    }
    Ok(samples)
}

fn benchmark_changed_subset(workload: Workload) -> BenchResult<Vec<Duration>> {
    let mut samples = Vec::with_capacity(workload.samples);
    for _ in 0..workload.samples {
        let fixture = TempDir::new()?;
        let source = fixture.path().join("source");
        create_fixture(&source, workload.files)?;
        let mut store = Store::open(fixture.path().join("store"))?;
        scan_workspace(&source, &mut store, &ScanOptions::default(), 0)?;
        change_fixture(&source, workload.changed_files)?;

        let started = Instant::now();
        let report = scan_workspace(&source, &mut store, &ScanOptions::default(), 1)?;
        samples.push(started.elapsed());
        black_box(report.snapshot.id);

        let expected = workload.files + workload.changed_files;
        if count_objects(&store)? != expected {
            return Err("changed-subset scan did not reuse unchanged objects".into());
        }
    }
    Ok(samples)
}

fn benchmark_materialization(workload: Workload) -> BenchResult<Vec<Duration>> {
    let fixture = TempDir::new()?;
    let source = fixture.path().join("source");
    create_fixture(&source, workload.files)?;
    let mut store = Store::open(fixture.path().join("store"))?;
    let scanned = scan_workspace(&source, &mut store, &ScanOptions::default(), 0)?;
    let mut samples = Vec::with_capacity(workload.samples);
    for sample in 0..workload.samples {
        let destination = fixture.path().join(format!("checkout-{sample}"));
        let started = Instant::now();
        let report = materialize(
            &scanned.snapshot,
            &store,
            destination,
            &MaterializeOptions::default(),
        )?;
        samples.push(started.elapsed());
        if usize::try_from(report.files)? != workload.files {
            return Err("materialization wrote an unexpected file count".into());
        }
        black_box(report.logical_bytes);
    }
    Ok(samples)
}

fn benchmark_checkpoint_commit(workload: Workload) -> BenchResult<Vec<Duration>> {
    let fixture = TempDir::new()?;
    let source = fixture.path().join("source");
    create_fixture(&source, workload.files)?;
    let mut store = Store::open(fixture.path().join("store"))?;
    let mut pending = Vec::with_capacity(workload.samples);
    for sample in 0..workload.samples {
        if sample > 0 {
            fs::write(
                fixture_path(&source, (sample - 1) % workload.files),
                format!("checkpoint-sample={sample}\n"),
            )?;
        }
        let snapshot = scan_workspace(
            &source,
            &mut store,
            &ScanOptions::default(),
            i64::try_from(sample)?,
        )?
        .snapshot;
        let run_id = RunId::generate();
        store.create_run(&benchmark_run(run_id, fixture.path()))?;
        pending.push((checkpoint(run_id, snapshot.id)?, snapshot));
    }

    let mut samples = Vec::with_capacity(workload.samples);
    for (checkpoint, snapshot) in pending {
        let started = Instant::now();
        store.commit_checkpoint(&checkpoint, &snapshot)?;
        samples.push(started.elapsed());
        black_box(store.load_checkpoint(checkpoint.id)?);
    }
    Ok(samples)
}

fn benchmark_run_comparison(workload: Workload) -> BenchResult<Vec<Duration>> {
    let fixture = TempDir::new()?;
    let source = fixture.path().join("source");
    create_fixture(&source, workload.files)?;
    let mut store = Store::open(fixture.path().join("store"))?;
    let left = scan_workspace(&source, &mut store, &ScanOptions::default(), 0)?.snapshot;
    change_fixture(&source, workload.changed_files)?;
    let right = scan_workspace(&source, &mut store, &ScanOptions::default(), 1)?.snapshot;
    let left_run = store_completed_run(&mut store, fixture.path(), &left)?;
    let right_run = store_completed_run(&mut store, fixture.path(), &right)?;

    let mut samples = Vec::with_capacity(workload.samples);
    for _ in 0..workload.samples {
        let started = Instant::now();
        let left_input = store.load_comparison_input(left_run)?;
        let right_input = store.load_comparison_input(right_run)?;
        let runs = store.list_runs()?;
        let left_snapshot = store.load_snapshot(
            left_input
                .run
                .final_snapshot
                .ok_or("benchmark left run has no final snapshot")?,
        )?;
        let right_snapshot = store.load_snapshot(
            right_input
                .run
                .final_snapshot
                .ok_or("benchmark right run has no final snapshot")?,
        )?;
        let diff = diff_snapshots(&left_snapshot, &right_snapshot);
        samples.push(started.elapsed());
        if diff.changes.len() != workload.changed_files {
            return Err("run comparison returned an unexpected change count".into());
        }
        black_box((left_input, right_input, runs, diff));
    }
    Ok(samples)
}

fn benchmark_timeline(workload: Workload) -> BenchResult<(Vec<Duration>, Vec<Duration>)> {
    let mut ingestion = Vec::with_capacity(workload.samples);
    let mut query = Vec::with_capacity(workload.samples);
    for _ in 0..workload.samples {
        let fixture = TempDir::new()?;
        let mut store = Store::open(fixture.path().join("store"))?;
        let run_id = RunId::generate();
        store.create_run(&benchmark_run(run_id, fixture.path()))?;
        let terminal_bytes = b"\x1b[32mbenchmark terminal frame\x1b[0m\r\n";
        let object_id = store.put_object(terminal_bytes, 0)?.id;
        let events = benchmark_events(
            run_id,
            workload.events,
            TerminalStreamId::generate(),
            object_id,
            u64::try_from(terminal_bytes.len())?,
        )?;

        let started = Instant::now();
        for batch in events.chunks(EVENT_BATCH) {
            store.append_event_batch(batch)?;
        }
        ingestion.push(started.elapsed());

        let started = Instant::now();
        let mut after = None;
        let mut loaded = 0_usize;
        loop {
            let page = store.load_timeline(run_id, after, TIMELINE_PAGE)?;
            loaded += page.events.len();
            after = page.events.last().map(|event| event.sequence);
            if !page.has_more {
                break;
            }
        }
        query.push(started.elapsed());
        if loaded != workload.events {
            return Err("timeline query returned an unexpected event count".into());
        }
        black_box(after);
    }
    Ok((ingestion, query))
}

fn create_fixture(root: &Path, files: usize) -> BenchResult {
    fs::create_dir(root)?;
    for index in 0..files {
        let path = fixture_path(root, index);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            format!(
                "file={index:08}\nvalue={:016x}\n",
                index.wrapping_mul(2_654_435_761)
            ),
        )?;
    }
    Ok(())
}

fn change_fixture(root: &Path, changed: usize) -> BenchResult {
    for index in 0..changed {
        fs::write(
            fixture_path(root, index),
            format!("file={index:08}\nchanged={:016x}\n", !index),
        )?;
    }
    Ok(())
}

fn fixture_path(root: &Path, index: usize) -> PathBuf {
    root.join(format!("group-{:02}", index % 32))
        .join(format!("file-{index:08}.txt"))
}

fn count_objects(store: &Store) -> BenchResult<usize> {
    let mut after = None;
    let mut count = 0_usize;
    loop {
        let page = store.list_objects(after, 10_000)?;
        count += page.objects.len();
        after = page.objects.last().map(|record| record.id);
        if !page.has_more {
            return Ok(count);
        }
    }
}

fn benchmark_run(id: RunId, workspace_root: &Path) -> Run {
    Run {
        id,
        branch_id: BranchId::generate(),
        parent: None,
        command: "benchmark-agent".to_owned(),
        arguments: Vec::new(),
        workspace_root: workspace_root.to_path_buf(),
        started_at: Timestamp::from_unix_milliseconds(0),
        finished_at: None,
        monotonic_duration: None,
        status: RunStatus::Preparing,
        platform: current_platform(),
        capture_policy: CapturePolicy::default(),
        initial_snapshot: None,
        final_snapshot: None,
        exit_status: None,
    }
}

fn checkpoint(run_id: RunId, snapshot_id: SnapshotId) -> BenchResult<Checkpoint> {
    Ok(Checkpoint {
        id: CheckpointId::generate(),
        run_id,
        sequence: EventSequence::new(1).ok_or("checkpoint sequence cannot be zero")?,
        label: None,
        reason: CheckpointReason::Initial,
        snapshot_id,
        created_at: Timestamp::from_unix_milliseconds(0),
        monotonic_offset: MonotonicDuration::from_nanoseconds(0),
    })
}

fn store_completed_run(
    store: &mut Store,
    workspace_root: &Path,
    snapshot: &Snapshot,
) -> BenchResult<RunId> {
    let run_id = RunId::generate();
    store.create_run(&benchmark_run(run_id, workspace_root))?;
    store.store_snapshot(snapshot, Timestamp::from_unix_milliseconds(0))?;
    store.mark_run_running(run_id, snapshot.id)?;
    store.finish_run(
        run_id,
        RunFinish {
            status: RunStatus::Completed,
            finished_at: Timestamp::from_unix_milliseconds(1),
            monotonic_duration: MonotonicDuration::from_nanoseconds(1),
            final_snapshot: Some(snapshot.id),
            exit_status: Some(ProcessExitStatus::Code(0)),
        },
    )?;
    Ok(run_id)
}

fn benchmark_events(
    run_id: RunId,
    count: usize,
    stream_id: TerminalStreamId,
    object_id: ObjectId,
    byte_len: u64,
) -> BenchResult<Vec<Event>> {
    let mut events = Vec::with_capacity(count);
    for index in 1..=count {
        let sequence = u64::try_from(index)?;
        events.push(Event::new(
            run_id,
            EventSequence::new(sequence).ok_or("event sequence cannot be zero")?,
            Timestamp::from_unix_milliseconds(i64::try_from(index)?),
            MonotonicDuration::from_nanoseconds(sequence),
            EventPayload::TerminalOutput {
                stream_id,
                object_id,
                byte_len,
            },
        )?);
    }
    Ok(events)
}

#[cfg(target_os = "macos")]
const fn current_platform() -> Platform {
    Platform::MacOsAarch64
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const fn current_platform() -> Platform {
    Platform::LinuxX86_64
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const fn current_platform() -> Platform {
    Platform::LinuxAarch64
}

fn report(name: &str, mut durations: Vec<Duration>) {
    durations.sort_unstable();
    let median = durations[durations.len() / 2];
    println!(
        "{name:30} median={median:?} min={:?} max={:?}",
        durations[0],
        durations[durations.len() - 1]
    );
}
