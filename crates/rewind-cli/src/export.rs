use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use rewind_capture::MAX_TERMINAL_CHUNK_BYTES;
use rewind_domain::{
    BranchId, CapturePolicy, Checkpoint, Event, EventPayload, EventSequence, InputRedactionReason,
    MonotonicDuration, ObjectId, Platform, ProcessExitStatus, Run, RunId, RunParent, RunStatus,
    Snapshot, SnapshotEntryKind, SnapshotId, Timestamp,
};
use rewind_snapshot::{EntryChange, diff_snapshots};
use rewind_store::{BundleError, BundleStreamWriter, Store, StoreError};
use serde::Serialize;
use thiserror::Error;

use crate::artifacts::{ArtifactError, atomic_write};
use crate::output::event_kind;

const EXPORT_PAGE_SIZE: u32 = 2_048;
const MAX_HTML_TERMINAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_HTML_EVENTS: usize = 1_000_000;

#[derive(Debug, Error)]
pub(crate) enum ExportError {
    #[error("run {0} has no initial snapshot")]
    MissingInitialSnapshot(RunId),
    #[error("run {0} has no final snapshot")]
    MissingFinalSnapshot(RunId),
    #[error(
        "HTML export contains more than {maximum} terminal bytes; use bundle export or lower capture limits"
    )]
    HtmlTooLarge { maximum: u64 },
    #[error(
        "HTML export contains more than {maximum} events; use bundle export to keep memory bounded"
    )]
    TooManyEvents { maximum: usize },
    #[error("cannot encode export metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bundle payload is {actual} bytes; configured maximum is {maximum}")]
    BundleTooLarge { actual: u64, maximum: u64 },
    #[error("terminal object {object_id} is {actual} bytes; event declares {expected}")]
    TerminalLength {
        object_id: ObjectId,
        expected: u64,
        actual: u64,
    },
    #[error("cannot write export at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Bundle(#[from] BundleError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BundleReport {
    pub(crate) path: PathBuf,
    pub(crate) entries: u32,
    pub(crate) payload_bytes: u64,
    pub(crate) redacted_input_events: u64,
}

pub(crate) fn export_bundle(
    store: &Store,
    run: &Run,
    output: Option<PathBuf>,
    redact_input: bool,
    maximum_bytes: u64,
) -> Result<BundleReport, ExportError> {
    let checkpoints = store.load_checkpoints(run.id)?;
    let mut snapshot_ids: BTreeSet<SnapshotId> = checkpoints
        .iter()
        .map(|checkpoint| checkpoint.snapshot_id)
        .collect();
    snapshot_ids.extend(run.initial_snapshot);
    snapshot_ids.extend(run.final_snapshot);

    let mut snapshots = BTreeMap::<SnapshotId, Snapshot>::new();
    let mut object_ids = BTreeSet::<ObjectId>::new();
    for snapshot_id in snapshot_ids {
        let snapshot = store.load_snapshot(snapshot_id)?;
        for entry in snapshot.manifest.entries() {
            if let SnapshotEntryKind::File { object_id, .. } = entry.kind {
                object_ids.insert(object_id);
            }
        }
        snapshots.insert(snapshot_id, snapshot);
    }

    let mut events_file = tempfile::NamedTempFile::new().map_err(|source| ExportError::Io {
        path: PathBuf::from("temporary bundle event stream"),
        source,
    })?;
    let mut event_hasher = blake3::Hasher::new();
    let mut event_bytes = 0_u64;
    let mut redacted_input_events = 0_u64;
    stream_export_events(
        store,
        run,
        redact_input,
        &mut events_file,
        &mut event_hasher,
        &mut event_bytes,
        &mut redacted_input_events,
        &mut object_ids,
        maximum_bytes,
    )?;
    events_file
        .as_file()
        .sync_all()
        .map_err(|source| ExportError::Io {
            path: events_file.path().to_path_buf(),
            source,
        })?;
    let event_checksum = *event_hasher.finalize().as_bytes();

    let checkpoints_bytes = serde_json::to_vec_pretty(&checkpoints)?;
    let run_view = ExportRunView::from(run);
    let run_bytes = serde_json::to_vec_pretty(&run_view)?;
    let mut snapshot_entries = BTreeMap::new();
    for (id, snapshot) in &snapshots {
        snapshot_entries.insert(
            format!("snapshots/{id}.json"),
            serde_json::to_vec(snapshot)?,
        );
    }
    let mut checksums = vec![
        checksum_view("checkpoints.json", &checkpoints_bytes),
        ChecksumView {
            path: "events.ndjson".to_owned(),
            blake3: hex(&event_checksum),
            bytes: event_bytes,
        },
        checksum_view("run.json", &run_bytes),
    ];
    for (path, bytes) in &snapshot_entries {
        checksums.push(checksum_view(path, bytes));
    }
    let mut object_sizes = BTreeMap::new();
    for id in &object_ids {
        let length = store.open_object_reader(*id, maximum_bytes)?.logical_size();
        let path = object_path(*id);
        checksums.push(ChecksumView {
            path,
            blake3: id.to_string(),
            bytes: length,
        });
        object_sizes.insert(*id, length);
    }
    checksums.sort_by(|left, right| left.path.cmp(&right.path));
    let manifest_bytes = serde_json::to_vec_pretty(&BundleManifest {
        format: "rewind-bundle",
        version: 1,
        run_id: run.id,
        event_encoding: "newline-delimited rewind-domain Event JSON",
        redaction: RedactionReport {
            terminal_input_events: redacted_input_events,
            policy: if redact_input {
                "omit_input"
            } else {
                "include_recorded_input"
            },
        },
        entries: &checksums,
    })?;

    let payload_bytes = checksums
        .iter()
        .try_fold(
            u64::try_from(manifest_bytes.len()).unwrap_or(u64::MAX),
            |total, entry| total.checked_add(entry.bytes),
        )
        .unwrap_or(u64::MAX);
    if payload_bytes > maximum_bytes {
        return Err(ExportError::BundleTooLarge {
            actual: payload_bytes,
            maximum: maximum_bytes,
        });
    }
    let entry_count = u32::try_from(
        4_usize
            .saturating_add(object_ids.len())
            .saturating_add(snapshot_entries.len()),
    )
    .map_err(|_| BundleError::NumericRange)?;
    let output = output.unwrap_or_else(|| default_bundle_path(run.id));
    write_bundle_file(
        &output,
        entry_count,
        &checkpoints_bytes,
        &events_file,
        event_bytes,
        event_checksum,
        &manifest_bytes,
        &object_ids,
        &object_sizes,
        store,
        &run_bytes,
        &snapshot_entries,
    )?;
    Ok(BundleReport {
        path: output,
        entries: entry_count,
        payload_bytes,
        redacted_input_events,
    })
}

pub(crate) fn export_html(
    store: &Store,
    run: &Run,
    output: Option<PathBuf>,
    redact_input: bool,
) -> Result<PathBuf, ExportError> {
    let initial_id = run
        .initial_snapshot
        .ok_or(ExportError::MissingInitialSnapshot(run.id))?;
    let final_id = run
        .final_snapshot
        .ok_or(ExportError::MissingFinalSnapshot(run.id))?;
    let initial = store.load_snapshot(initial_id)?;
    let final_snapshot = store.load_snapshot(final_id)?;
    let changes = diff_snapshots(&initial, &final_snapshot)
        .changes
        .into_iter()
        .map(ChangeView::from)
        .collect();
    let checkpoints = store.load_checkpoints(run.id)?;
    let (events, frames, terminal_bytes) = load_html_events(store, run.id, redact_input)?;
    let data = HtmlData {
        schema_version: 1,
        run: ExportRunView::from(run),
        checkpoints: &checkpoints,
        changes,
        events,
        frames,
        terminal_bytes,
        input_policy: if redact_input {
            "terminal input omitted from export"
        } else {
            "recorded terminal input remains in event metadata; bytes are not rendered"
        },
    };
    let json = safe_script_json(&data)?;
    let mut html = String::with_capacity(
        json.len()
            .saturating_add(HTML_PREFIX.len() + HTML_SUFFIX.len()),
    );
    html.push_str(HTML_PREFIX);
    html.push_str(&json);
    html.push_str(HTML_SUFFIX);
    let output = output.unwrap_or_else(|| PathBuf::from(format!("rewind-{}.html", run.id)));
    atomic_write(&output, html.as_bytes())?;
    Ok(output)
}

fn load_html_events(
    store: &Store,
    run_id: RunId,
    redact_input: bool,
) -> Result<(Vec<EventView>, Vec<TerminalFrame>, u64), ExportError> {
    let mut cursor: Option<EventSequence> = None;
    let mut events = Vec::new();
    let mut frames = Vec::new();
    let mut terminal_bytes = 0_u64;
    loop {
        let page = store.load_timeline(run_id, cursor, EXPORT_PAGE_SIZE)?;
        for event in &page.events {
            if events.len() == MAX_HTML_EVENTS {
                return Err(ExportError::TooManyEvents {
                    maximum: MAX_HTML_EVENTS,
                });
            }
            let redacted = redact_input
                && matches!(
                    &event.payload,
                    EventPayload::TerminalInput { .. } | EventPayload::TerminalInputRedacted { .. }
                );
            events.push(EventView {
                sequence: event.sequence.get(),
                offset_ms: event.monotonic_offset.as_nanoseconds() / 1_000_000,
                kind: event_kind(&event.payload),
                redacted,
            });
            if let EventPayload::TerminalOutput {
                object_id,
                byte_len,
                ..
            } = &event.payload
            {
                terminal_bytes =
                    terminal_bytes
                        .checked_add(*byte_len)
                        .ok_or(ExportError::HtmlTooLarge {
                            maximum: MAX_HTML_TERMINAL_BYTES,
                        })?;
                if terminal_bytes > MAX_HTML_TERMINAL_BYTES {
                    return Err(ExportError::HtmlTooLarge {
                        maximum: MAX_HTML_TERMINAL_BYTES,
                    });
                }
                frames.push(TerminalFrame {
                    sequence: event.sequence.get(),
                    offset_ms: event.monotonic_offset.as_nanoseconds() / 1_000_000,
                    hex: {
                        let bytes =
                            store.load_object(*object_id, MAX_TERMINAL_CHUNK_BYTES as u64)?;
                        let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                        if actual != *byte_len {
                            return Err(ExportError::TerminalLength {
                                object_id: *object_id,
                                expected: *byte_len,
                                actual,
                            });
                        }
                        hex(&bytes)
                    },
                });
            }
            cursor = Some(event.sequence);
        }
        if !page.has_more {
            break;
        }
    }
    Ok((events, frames, terminal_bytes))
}

#[allow(clippy::too_many_arguments)]
fn stream_export_events(
    store: &Store,
    run: &Run,
    redact_input: bool,
    output: &mut tempfile::NamedTempFile,
    hasher: &mut blake3::Hasher,
    total_bytes: &mut u64,
    redacted_count: &mut u64,
    object_ids: &mut BTreeSet<ObjectId>,
    maximum_bytes: u64,
) -> Result<(), ExportError> {
    let mut cursor: Option<EventSequence> = None;
    loop {
        let page = store.load_timeline(run.id, cursor, EXPORT_PAGE_SIZE)?;
        for original in &page.events {
            let mut event = original.clone();
            match &original.payload {
                EventPayload::TerminalInput {
                    stream_id,
                    byte_len,
                    ..
                } if redact_input => {
                    event.payload = EventPayload::TerminalInputRedacted {
                        stream_id: *stream_id,
                        byte_len: *byte_len,
                        reason: InputRedactionReason::ExportPolicy,
                    };
                    *redacted_count = redacted_count.saturating_add(1);
                }
                EventPayload::TerminalInput { object_id, .. }
                | EventPayload::TerminalOutput { object_id, .. } => {
                    object_ids.insert(*object_id);
                }
                EventPayload::RunStarted { .. }
                | EventPayload::WorkspaceIsolated { .. }
                | EventPayload::TerminalInputRedacted { .. }
                | EventPayload::TerminalResized { .. }
                | EventPayload::ProcessObserved { .. }
                | EventPayload::ProcessExited { .. }
                | EventPayload::FilesystemPathsDirtied { .. }
                | EventPayload::CheckpointStarted { .. }
                | EventPayload::CheckpointCommitted { .. }
                | EventPayload::CheckpointFailed { .. }
                | EventPayload::MarkerCreated { .. }
                | EventPayload::RunInterrupted { .. }
                | EventPayload::RunCompleted { .. }
                | EventPayload::RecorderWarning { .. } => {}
            }
            sanitize_export_event(&mut event, &run.workspace_root);
            write_event_line(output, hasher, total_bytes, &event, maximum_bytes)?;
            cursor = Some(original.sequence);
        }
        if !page.has_more {
            break;
        }
    }
    Ok(())
}

fn sanitize_export_event(event: &mut Event, workspace_root: &Path) {
    match &mut event.payload {
        EventPayload::ProcessObserved { process } => {
            let Some(executable) = &process.executable else {
                return;
            };
            let path = Path::new(executable);
            let Ok(relative) = path.strip_prefix(workspace_root) else {
                return;
            };
            process.executable = Some(if relative.as_os_str().is_empty() {
                "<workspace>".to_owned()
            } else {
                format!("<workspace>/{}", relative.display())
            });
        }
        EventPayload::RecorderWarning { warning } => {
            warning.message = redact_workspace_root(&warning.message, workspace_root);
        }
        EventPayload::CheckpointFailed { failure, .. } => {
            failure.message = redact_workspace_root(&failure.message, workspace_root);
        }
        EventPayload::RunStarted { .. }
        | EventPayload::WorkspaceIsolated { .. }
        | EventPayload::TerminalInput { .. }
        | EventPayload::TerminalInputRedacted { .. }
        | EventPayload::TerminalOutput { .. }
        | EventPayload::TerminalResized { .. }
        | EventPayload::ProcessExited { .. }
        | EventPayload::FilesystemPathsDirtied { .. }
        | EventPayload::CheckpointStarted { .. }
        | EventPayload::CheckpointCommitted { .. }
        | EventPayload::MarkerCreated { .. }
        | EventPayload::RunInterrupted { .. }
        | EventPayload::RunCompleted { .. } => {}
    }
}

fn redact_workspace_root(message: &str, workspace_root: &Path) -> String {
    workspace_root.to_str().map_or_else(
        || message.to_owned(),
        |root| message.replace(root, "<workspace>"),
    )
}

fn write_event_line(
    output: &mut tempfile::NamedTempFile,
    hasher: &mut blake3::Hasher,
    total_bytes: &mut u64,
    event: &Event,
    maximum_bytes: u64,
) -> Result<(), ExportError> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    let line_length = u64::try_from(line.len()).unwrap_or(u64::MAX);
    *total_bytes = total_bytes.checked_add(line_length).unwrap_or(u64::MAX);
    if *total_bytes > maximum_bytes {
        return Err(ExportError::BundleTooLarge {
            actual: *total_bytes,
            maximum: maximum_bytes,
        });
    }
    output.write_all(&line).map_err(|source| ExportError::Io {
        path: output.path().to_path_buf(),
        source,
    })?;
    hasher.update(&line);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_bundle_file(
    output: &Path,
    entry_count: u32,
    checkpoints: &[u8],
    events: &tempfile::NamedTempFile,
    events_length: u64,
    events_checksum: [u8; 32],
    manifest: &[u8],
    object_ids: &BTreeSet<ObjectId>,
    object_sizes: &BTreeMap<ObjectId, u64>,
    store: &Store,
    run: &[u8],
    snapshots: &BTreeMap<String, Vec<u8>>,
) -> Result<(), ExportError> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ExportError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| ExportError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    {
        let mut writer = BundleStreamWriter::new(temporary.as_file_mut(), entry_count)?;
        writer.write_entry("checkpoints.json", checkpoints)?;
        let event_reader = File::open(events.path()).map_err(|source| ExportError::Io {
            path: events.path().to_path_buf(),
            source,
        })?;
        writer.write_prehashed_entry(
            "events.ndjson",
            events_length,
            events_checksum,
            event_reader,
        )?;
        writer.write_entry("manifest.json", manifest)?;
        for object_id in object_ids {
            let expected = object_sizes.get(object_id).copied().unwrap_or(u64::MAX);
            let reader = store.open_object_reader(*object_id, expected)?;
            if reader.logical_size() != expected {
                return Err(ExportError::Store(StoreError::Invariant {
                    message: format!("object {object_id} changed size during bundle export"),
                }));
            }
            writer.write_prehashed_entry(
                &object_path(*object_id),
                expected,
                *object_id.as_bytes(),
                reader,
            )?;
        }
        writer.write_entry("run.json", run)?;
        for (path, bytes) in snapshots {
            writer.write_entry(path, bytes)?;
        }
        let file = writer.finish()?;
        file.sync_all().map_err(|source| ExportError::Io {
            path: output.to_path_buf(),
            source,
        })?;
    }
    temporary.persist(output).map_err(|error| ExportError::Io {
        path: output.to_path_buf(),
        source: error.error,
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ExportError::Io {
            path: parent.to_path_buf(),
            source,
        })
}

fn object_path(id: ObjectId) -> String {
    let digest = id.to_string();
    format!("objects/{}/{}", &digest[..2], &digest[2..])
}

fn checksum_view(path: &str, bytes: &[u8]) -> ChecksumView {
    ChecksumView {
        path: path.to_owned(),
        blake3: blake3::hash(bytes).to_hex().to_string(),
        bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    }
}

#[derive(Serialize)]
struct BundleManifest<'a> {
    format: &'static str,
    version: u16,
    run_id: RunId,
    event_encoding: &'static str,
    redaction: RedactionReport,
    entries: &'a [ChecksumView],
}

#[derive(Serialize)]
struct RedactionReport {
    terminal_input_events: u64,
    policy: &'static str,
}

#[derive(Serialize)]
struct ChecksumView {
    path: String,
    blake3: String,
    bytes: u64,
}

fn safe_script_json(value: &impl Serialize) -> Result<String, serde_json::Error> {
    Ok(serde_json::to_string(value)?
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

#[derive(Serialize)]
struct HtmlData<'a> {
    schema_version: u16,
    run: ExportRunView<'a>,
    checkpoints: &'a [Checkpoint],
    changes: Vec<ChangeView>,
    events: Vec<EventView>,
    frames: Vec<TerminalFrame>,
    terminal_bytes: u64,
    input_policy: &'static str,
}

#[derive(Serialize)]
struct ExportRunView<'a> {
    id: RunId,
    branch_id: BranchId,
    parent: Option<RunParent>,
    command: &'a str,
    arguments: &'a [String],
    started_at: Timestamp,
    finished_at: Option<Timestamp>,
    monotonic_duration: Option<MonotonicDuration>,
    status: RunStatus,
    platform: Platform,
    capture_policy: CapturePolicy,
    initial_snapshot: Option<SnapshotId>,
    final_snapshot: Option<SnapshotId>,
    exit_status: Option<ProcessExitStatus>,
}

impl<'a> From<&'a Run> for ExportRunView<'a> {
    fn from(run: &'a Run) -> Self {
        Self {
            id: run.id,
            branch_id: run.branch_id,
            parent: run.parent,
            command: &run.command,
            arguments: &run.arguments,
            started_at: run.started_at,
            finished_at: run.finished_at,
            monotonic_duration: run.monotonic_duration,
            status: run.status,
            platform: run.platform,
            capture_policy: run.capture_policy,
            initial_snapshot: run.initial_snapshot,
            final_snapshot: run.final_snapshot,
            exit_status: run.exit_status,
        }
    }
}

#[derive(Serialize)]
struct EventView {
    sequence: u64,
    offset_ms: u64,
    kind: &'static str,
    redacted: bool,
}

#[derive(Serialize)]
struct TerminalFrame {
    sequence: u64,
    offset_ms: u64,
    hex: String,
}

#[derive(Serialize)]
struct ChangeView {
    path: String,
    status: &'static str,
}

impl From<EntryChange> for ChangeView {
    fn from(value: EntryChange) -> Self {
        match value {
            EntryChange::Added { entry } => Self {
                path: entry.path.into_string(),
                status: "added",
            },
            EntryChange::Removed { entry } => Self {
                path: entry.path.into_string(),
                status: "deleted",
            },
            EntryChange::Modified { before, .. } => Self {
                path: before.path.into_string(),
                status: "modified",
            },
        }
    }
}

pub(crate) fn default_bundle_path(run: RunId) -> PathBuf {
    PathBuf::from(format!("rewind-{run}.rwbundle"))
}

const HTML_PREFIX: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Rewind replay</title><style>
:root{color-scheme:dark;--bg:#111418;--panel:#181c22;--line:#303641;--muted:#9ca6b5;--text:#e4e9f0;--accent:#82b3ff}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:13px ui-monospace,SFMono-Regular,Menlo,monospace}
header{padding:14px 18px;border-bottom:1px solid var(--line);display:flex;gap:24px;align-items:baseline}h1{font-size:16px;margin:0}small{color:var(--muted)}
main{display:grid;grid-template-columns:280px 1fr;height:calc(100vh - 48px)}aside{border-right:1px solid var(--line);overflow:auto;padding:12px}.content{display:grid;grid-template-rows:1fr 220px;min-width:0}
.terminal{background:#080a0d;padding:14px;overflow:auto;white-space:pre-wrap;word-break:break-word;font-size:12px}.bottom{display:grid;grid-template-columns:1fr 1fr;border-top:1px solid var(--line);overflow:hidden}.pane{padding:12px;overflow:auto}.pane+.pane{border-left:1px solid var(--line)}
h2{font-size:12px;text-transform:uppercase;color:var(--muted);margin:0 0 9px}ul{list-style:none;padding:0;margin:0}li{padding:3px 0}.status{color:var(--accent)}
.controls{display:flex;gap:8px;align-items:center;margin:10px 0}button,input,select{font:inherit;color:var(--text);background:var(--panel);border:1px solid var(--line);padding:4px 7px}input[type=range]{width:100%}
.ansi-bold{font-weight:700}.c30{color:#555}.c31{color:#e06c75}.c32{color:#98c379}.c33{color:#e5c07b}.c34{color:#61afef}.c35{color:#c678dd}.c36{color:#56b6c2}.c37{color:#d7dae0}.c90{color:#7f8793}.c91{color:#ff7a85}.c92{color:#b6e890}.c93{color:#ffd68a}.c94{color:#83c3ff}.c95{color:#e8a0ff}.c96{color:#7de1ec}.c97{color:#fff}
</style></head><body><header><h1>Rewind replay</h1><small id="run-title"></small></header><main><aside><h2>Run</h2><div id="meta"></div><div class="controls"><button id="play">Play</button><select id="speed"><option>.5</option><option selected>1</option><option>2</option><option>4</option></select></div><input id="seek" type="range" min="0" value="0"><small id="clock"></small><h2 style="margin-top:16px">Checkpoints</h2><ul id="checkpoints"></ul></aside><section class="content"><div id="terminal" class="terminal" aria-label="Recorded terminal output"></div><div class="bottom"><div class="pane"><h2>Changed files</h2><ul id="changes"></ul></div><div class="pane"><h2>Timeline</h2><ul id="events"></ul></div></div></section></main>
<script id="rewind-data" type="application/json">"#;

const HTML_SUFFIX: &str = r#"</script><script>
'use strict';const d=JSON.parse(document.getElementById('rewind-data').textContent);const $=id=>document.getElementById(id);
$('run-title').textContent=d.run.id+' · '+d.run.status+' · '+d.run.command;$('meta').textContent='Parent: '+(d.run.parent?d.run.parent.run_id+'@'+d.run.parent.checkpoint_id:'none')+'\nTerminal: '+d.terminal_bytes+' bytes\nPrivacy: '+d.input_policy;
d.checkpoints.forEach(c=>{const li=document.createElement('li');li.textContent=c.sequence+'  '+c.reason+(c.label?'  '+c.label:'');$('checkpoints').append(li)});d.changes.forEach(c=>{const li=document.createElement('li');li.textContent=c.status.padEnd(9)+' '+c.path;$('changes').append(li)});
const max=d.frames.length?d.frames[d.frames.length-1].offset_ms:0;$('seek').max=max;let position=0,timer=null,last=performance.now();
function bytes(hex){const a=new Uint8Array(hex.length/2);for(let i=0;i<a.length;i++)a[i]=parseInt(hex.slice(i*2,i*2+2),16);return a}
function renderAnsi(text,node){node.replaceChildren();let style={bold:false,color:''},at=0,re=/\x1b\[([0-9;?]*)([ -\/]*)([@-~])/g,m;const add=s=>{if(!s)return;const span=document.createElement('span');if(style.bold)span.classList.add('ansi-bold');if(style.color)span.classList.add(style.color);span.textContent=s;node.append(span)};while((m=re.exec(text))){add(text.slice(at,m.index));if(m[3]==='m'){const codes=(m[1]||'0').split(';').map(Number);for(const c of codes){if(c===0)style={bold:false,color:''};else if(c===1)style.bold=true;else if((c>=30&&c<=37)||(c>=90&&c<=97))style.color='c'+c}}at=re.lastIndex}add(text.slice(at))}
function render(){const parts=[];let length=0;for(const f of d.frames){if(f.offset_ms>position)break;const part=bytes(f.hex);parts.push(part);length+=part.length}const joined=new Uint8Array(length);let at=0;for(const part of parts){joined.set(part,at);at+=part.length}const text=new TextDecoder().decode(joined);renderAnsi(text.replace(/\r\n/g,'\n').replace(/\r/g,'\n'),$('terminal'));$('terminal').scrollTop=$('terminal').scrollHeight;$('seek').value=position;$('clock').textContent=(position/1000).toFixed(2)+'s / '+(max/1000).toFixed(2)+'s';$('events').replaceChildren();d.events.filter(e=>Math.abs(e.offset_ms-position)<1000).slice(-30).forEach(e=>{const li=document.createElement('li');li.textContent=e.sequence+'  '+e.kind+(e.redacted?' [redacted]':'');$('events').append(li)})}
function tick(now){position=Math.min(max,position+(now-last)*Number($('speed').value));last=now;render();if(position>=max){pause()}else timer=requestAnimationFrame(tick)}function play(){if(timer)return;$('play').textContent='Pause';last=performance.now();timer=requestAnimationFrame(tick)}function pause(){if(timer)cancelAnimationFrame(timer);timer=null;$('play').textContent='Play'}
$('play').onclick=()=>timer?pause():play();$('seek').oninput=e=>{position=Number(e.target.value);render()};render();
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use rewind_domain::{
        ProcessId, ProcessObservation, ProcessRelationship, RecorderWarning, RecorderWarningCode,
    };

    #[test]
    fn embedded_json_cannot_close_the_script_element() {
        let encoded =
            safe_script_json(&BTreeMap::from([("value", "</script><b>bad</b>")])).unwrap();
        assert!(!encoded.contains('<'));
        assert!(encoded.contains("\\u003c/script\\u003e"));
    }

    #[test]
    fn exported_run_metadata_omits_local_workspace_paths() {
        let run = Run {
            id: RunId::generate(),
            branch_id: BranchId::generate(),
            parent: None,
            command: "agent".to_owned(),
            arguments: Vec::new(),
            workspace_root: PathBuf::from("/Users/alice/private/rewind/workspace"),
            started_at: Timestamp::from_unix_milliseconds(1),
            finished_at: None,
            monotonic_duration: None,
            status: RunStatus::Preparing,
            platform: Platform::MacOsAarch64,
            capture_policy: CapturePolicy::default(),
            initial_snapshot: None,
            final_snapshot: None,
            exit_status: None,
        };

        let json = serde_json::to_string(&ExportRunView::from(&run)).unwrap();
        assert!(!json.contains("workspace_root"));
        assert!(!json.contains("/Users/alice"));
    }

    #[test]
    fn exported_events_relativize_workspace_executables_and_diagnostics() {
        let workspace = PathBuf::from("/Users/alice/private/rewind/workspace");
        let mut process = Event::new(
            RunId::generate(),
            EventSequence::FIRST,
            Timestamp::from_unix_milliseconds(1),
            MonotonicDuration::ZERO,
            EventPayload::ProcessObserved {
                process: ProcessObservation {
                    process_id: ProcessId::new(7).unwrap(),
                    parent_process_id: None,
                    executable: Some(
                        workspace
                            .join("target/debug/agent")
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    command: "agent".to_owned(),
                    relationship: ProcessRelationship::Root,
                },
            },
        )
        .unwrap();
        let mut warning = Event::new(
            process.run_id,
            EventSequence::new(2).unwrap(),
            Timestamp::from_unix_milliseconds(2),
            MonotonicDuration::from_nanoseconds(1),
            EventPayload::RecorderWarning {
                warning: RecorderWarning {
                    code: RecorderWarningCode::Other,
                    message: format!("failed beneath {}", workspace.display()),
                },
            },
        )
        .unwrap();

        sanitize_export_event(&mut process, &workspace);
        sanitize_export_event(&mut warning, &workspace);
        let json = serde_json::to_string(&(process, warning)).unwrap();

        assert!(!json.contains("/Users/alice"));
        assert!(json.contains("<workspace>/target/debug/agent"));
        assert!(json.contains("failed beneath <workspace>"));
    }
}
