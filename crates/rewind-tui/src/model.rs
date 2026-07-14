use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::time::Instant;

use rewind_domain::{
    Checkpoint, CheckpointId, Event, EventPayload, EventSequence, InputRedactionReason, ObjectId,
    ProcessExitStatus, ProcessId, ProcessObservation, RecorderFailureKind, RecorderWarningCode,
    Run, RunId, RunStatus, SnapshotEntryKind, SnapshotId,
};
use rewind_snapshot::{EntryChange, diff_snapshots};
use rewind_store::{Store, TimelinePage, WarningRecord};

use crate::terminal::TerminalDocument;
use crate::{ReplayError, Result};

pub(crate) const TIMELINE_PAGE_SIZE: u32 = 256;
const MAX_TERMINAL_VIEW_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACE_CHANGES: usize = 10_000;
const MAX_DIFF_FILE_BYTES: u64 = 256 * 1024;
const MAX_DIFF_LINES: usize = 400;
const MAX_PROCESS_RECORDS: usize = 4_096;
const MAX_WARNINGS: usize = 1_024;

const SPEEDS: &[(u32, u32, &str)] = &[
    (1, 4, "0.25×"),
    (1, 2, "0.5×"),
    (1, 1, "1×"),
    (2, 1, "2×"),
    (4, 1, "4×"),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Focus {
    Runs,
    Timeline,
    Terminal,
    Workspace,
    Processes,
    Details,
}

impl Focus {
    pub(crate) const ALL: [Self; 6] = [
        Self::Runs,
        Self::Timeline,
        Self::Terminal,
        Self::Workspace,
        Self::Processes,
        Self::Details,
    ];

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Runs => "Runs",
            Self::Timeline => "Timeline",
            Self::Terminal => "Terminal",
            Self::Workspace => "Workspace",
            Self::Processes => "Processes",
            Self::Details => "Details",
        }
    }

    pub(crate) fn next(self, reverse: bool) -> Self {
        let position = Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        let next = if reverse {
            position.checked_sub(1).unwrap_or(Self::ALL.len() - 1)
        } else {
            (position + 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RunRow {
    pub(crate) run_id: RunId,
    pub(crate) depth: usize,
    pub(crate) status: RunStatus,
    pub(crate) command: String,
    pub(crate) duration_ns: Option<u64>,
    pub(crate) parent: Option<(RunId, CheckpointId)>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessRecord {
    pub(crate) process: ProcessObservation,
    pub(crate) first_sequence: EventSequence,
    pub(crate) exit: Option<(EventSequence, ProcessExitStatus)>,
}

#[derive(Clone, Debug)]
pub(crate) enum WorkspaceState {
    Unloaded {
        checkpoint: Option<CheckpointId>,
    },
    Loaded {
        checkpoint: Option<CheckpointId>,
        snapshot_id: SnapshotId,
        changes: Vec<EntryChange>,
        total_changes: usize,
        selected: usize,
        preview: Vec<String>,
        preview_scroll: u16,
    },
    Unavailable {
        checkpoint: Option<CheckpointId>,
        message: String,
    },
}

impl WorkspaceState {
    pub(crate) const fn checkpoint(&self) -> Option<CheckpointId> {
        match self {
            Self::Unloaded { checkpoint }
            | Self::Loaded { checkpoint, .. }
            | Self::Unavailable { checkpoint, .. } => *checkpoint,
        }
    }
}

pub(crate) struct App {
    store: Store,
    pub(crate) run: Run,
    pub(crate) run_rows: Vec<RunRow>,
    pub(crate) events: Vec<Event>,
    pub(crate) event_selected: usize,
    page_has_more: bool,
    pending_page: Option<TimelinePage>,
    pub(crate) checkpoints: Vec<Checkpoint>,
    pub(crate) warnings: Vec<WarningRecord>,
    warnings_truncated: bool,
    pub(crate) processes: BTreeMap<ProcessId, ProcessRecord>,
    pub(crate) process_selected: usize,
    pub(crate) workspace: WorkspaceState,
    terminal_cache: ObjectCache,
    pub(crate) terminal: TerminalDocument,
    pub(crate) focus: Focus,
    pub(crate) playing: bool,
    speed_index: usize,
    last_tick: Instant,
    playback_target_ns: u128,
    pub(crate) show_help: bool,
    pub(crate) status: Option<String>,
    pub(crate) should_quit: bool,
}

impl App {
    pub(crate) fn load(store_root: &Path, run_id: RunId, cache_bytes: usize) -> Result<Self> {
        if cache_bytes == 0 {
            return Err(ReplayError::InvalidTerminalCache);
        }
        let store = Store::open_read_only(store_root)?;
        let run = store.load_run(run_id)?;
        let run_rows = build_run_rows(store.list_runs()?, run_id);
        let checkpoints = store.load_checkpoints(run_id)?;
        let mut warnings = store.load_warnings(run_id)?;
        let warnings_truncated = warnings.len() > MAX_WARNINGS;
        if warnings_truncated {
            warnings.drain(..warnings.len() - MAX_WARNINGS);
        }
        let page = store.load_timeline(run_id, None, TIMELINE_PAGE_SIZE)?;
        let mut app = Self {
            store,
            run,
            run_rows,
            events: page.events,
            event_selected: 0,
            page_has_more: page.has_more,
            pending_page: None,
            checkpoints,
            warnings,
            warnings_truncated,
            processes: BTreeMap::new(),
            process_selected: 0,
            workspace: WorkspaceState::Unloaded { checkpoint: None },
            terminal_cache: ObjectCache::new(cache_bytes),
            terminal: TerminalDocument::default(),
            focus: Focus::Timeline,
            playing: false,
            speed_index: 2,
            last_tick: Instant::now(),
            playback_target_ns: 0,
            show_help: false,
            status: None,
            should_quit: false,
        };
        app.ingest_current_page();
        app.sync_checkpoint();
        app.sync_playback_target();
        app.refresh_terminal()?;
        Ok(app)
    }

    pub(crate) fn selected_event(&self) -> Option<&Event> {
        self.events.get(self.event_selected)
    }

    pub(crate) fn selected_sequence(&self) -> Option<EventSequence> {
        self.selected_event().map(|event| event.sequence)
    }

    pub(crate) fn page_range(&self) -> Option<(EventSequence, EventSequence, bool)> {
        Some((
            self.events.first()?.sequence,
            self.events.last()?.sequence,
            self.page_has_more,
        ))
    }

    pub(crate) fn speed_label(&self) -> &'static str {
        SPEEDS[self.speed_index].2
    }

    pub(crate) fn warnings_truncated(&self) -> bool {
        self.warnings_truncated
    }

    pub(crate) fn cycle_focus(&mut self, reverse: bool) {
        self.focus = self.focus.next(reverse);
    }

    pub(crate) fn toggle_playback(&mut self) {
        self.playing = !self.playing;
        self.last_tick = Instant::now();
        self.sync_playback_target();
    }

    pub(crate) fn change_speed(&mut self, faster: bool) {
        self.speed_index = if faster {
            (self.speed_index + 1).min(SPEEDS.len() - 1)
        } else {
            self.speed_index.saturating_sub(1)
        };
        self.last_tick = Instant::now();
    }

    pub(crate) fn tick(&mut self, now: Instant) -> Result<()> {
        if !self.playing {
            self.last_tick = now;
            return Ok(());
        }
        let elapsed = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        let (numerator, denominator, _) = SPEEDS[self.speed_index];
        let scaled =
            elapsed.as_nanos().saturating_mul(u128::from(numerator)) / u128::from(denominator);
        self.playback_target_ns = self.playback_target_ns.saturating_add(scaled);

        let mut moved = false;
        for _ in 0..64 {
            let Some(next_offset) = self.peek_next_offset()? else {
                self.playing = false;
                break;
            };
            if u128::from(next_offset) > self.playback_target_ns {
                break;
            }
            if !self.step_next_raw()? {
                self.playing = false;
                break;
            }
            moved = true;
        }
        if moved {
            self.sync_checkpoint();
            self.refresh_terminal()?;
        }
        Ok(())
    }

    pub(crate) fn step_event(&mut self, forward: bool) -> Result<()> {
        self.playing = false;
        let moved = if forward {
            self.step_next_raw()?
        } else {
            self.step_previous_raw()?
        };
        if moved {
            self.after_event_move()?;
        }
        Ok(())
    }

    pub(crate) fn step_checkpoint(&mut self, forward: bool) -> Result<()> {
        self.playing = false;
        let current = self.selected_sequence().map_or(0, EventSequence::get);
        let target = if forward {
            self.checkpoints
                .iter()
                .find(|checkpoint| checkpoint.sequence.get() > current)
        } else {
            self.checkpoints
                .iter()
                .rev()
                .find(|checkpoint| checkpoint.sequence.get() < current)
        }
        .map(|checkpoint| checkpoint.sequence);
        if let Some(target) = target {
            self.jump_to(target)?;
            self.after_event_move()?;
        }
        Ok(())
    }

    pub(crate) fn move_selection(&mut self, down: bool) -> Result<()> {
        match self.focus {
            Focus::Timeline | Focus::Terminal | Focus::Details => self.step_event(down)?,
            Focus::Workspace => self.move_workspace_selection(down)?,
            Focus::Processes => self.move_process_selection(down),
            Focus::Runs => {}
        }
        Ok(())
    }

    pub(crate) fn activate(&mut self) -> Result<()> {
        if self.focus == Focus::Workspace {
            self.load_workspace()?;
        }
        Ok(())
    }

    pub(crate) fn scroll_workspace_preview(&mut self, down: bool) {
        if self.focus != Focus::Workspace {
            return;
        }
        if let WorkspaceState::Loaded { preview_scroll, .. } = &mut self.workspace {
            *preview_scroll = if down {
                preview_scroll.saturating_add(10)
            } else {
                preview_scroll.saturating_sub(10)
            };
        }
    }

    pub(crate) fn visible_processes(&self) -> Vec<&ProcessRecord> {
        let sequence = self.selected_sequence().map_or(0, EventSequence::get);
        self.processes
            .values()
            .filter(|record| record.first_sequence.get() <= sequence)
            .collect()
    }

    pub(crate) fn selected_process(&self) -> Option<&ProcessRecord> {
        self.visible_processes().get(self.process_selected).copied()
    }

    pub(crate) fn current_checkpoint(&self) -> Option<&Checkpoint> {
        let sequence = self.selected_sequence()?.get();
        self.checkpoints
            .iter()
            .rev()
            .find(|checkpoint| checkpoint.sequence.get() <= sequence)
    }

    pub(crate) fn checkpoint_at(&self, sequence: EventSequence) -> Option<&Checkpoint> {
        self.checkpoints
            .binary_search_by_key(&sequence, |checkpoint| checkpoint.sequence)
            .ok()
            .map(|index| &self.checkpoints[index])
    }

    fn move_workspace_selection(&mut self, down: bool) -> Result<()> {
        let WorkspaceState::Loaded {
            changes,
            selected,
            preview_scroll,
            ..
        } = &mut self.workspace
        else {
            return Ok(());
        };
        if changes.is_empty() {
            *selected = 0;
        } else if down {
            *selected = (*selected + 1).min(changes.len() - 1);
        } else {
            *selected = selected.saturating_sub(1);
        }
        *preview_scroll = 0;
        let change = changes.get(*selected).cloned();
        let preview = change
            .as_ref()
            .map(|change| self.workspace_preview(change))
            .transpose()?
            .unwrap_or_default();
        if let WorkspaceState::Loaded {
            preview: current, ..
        } = &mut self.workspace
        {
            *current = preview;
        }
        Ok(())
    }

    fn move_process_selection(&mut self, down: bool) {
        let len = self.visible_processes().len();
        if len == 0 {
            self.process_selected = 0;
        } else if down {
            self.process_selected = (self.process_selected + 1).min(len - 1);
        } else {
            self.process_selected = self.process_selected.saturating_sub(1);
        }
    }

    fn load_workspace(&mut self) -> Result<()> {
        let checkpoint = self.current_checkpoint().cloned();
        let checkpoint_id = checkpoint.as_ref().map(|value| value.id);
        let Some(initial_id) = self.run.initial_snapshot else {
            self.workspace = WorkspaceState::Unavailable {
                checkpoint: checkpoint_id,
                message: "run has no initial snapshot".to_owned(),
            };
            return Ok(());
        };
        let selected_id = checkpoint
            .as_ref()
            .map_or(initial_id, |value| value.snapshot_id);
        let initial = self.store.load_snapshot(initial_id)?;
        let mut changes = if selected_id == initial_id {
            diff_snapshots(&initial, &initial).changes
        } else {
            let selected = self.store.load_snapshot(selected_id)?;
            diff_snapshots(&initial, &selected).changes
        };
        let total_changes = changes.len();
        changes.truncate(MAX_WORKSPACE_CHANGES);
        let preview = changes
            .first()
            .map(|change| self.workspace_preview(change))
            .transpose()?
            .unwrap_or_default();
        self.workspace = WorkspaceState::Loaded {
            checkpoint: checkpoint_id,
            snapshot_id: selected_id,
            changes,
            total_changes,
            selected: 0,
            preview,
            preview_scroll: 0,
        };
        Ok(())
    }

    fn workspace_preview(&self, change: &EntryChange) -> Result<Vec<String>> {
        let path = change.path().as_str();
        let (before, after) = match change {
            EntryChange::Added { entry } => (PreviewFile::Absent, self.preview_file(&entry.kind)?),
            EntryChange::Removed { entry } => {
                (self.preview_file(&entry.kind)?, PreviewFile::Absent)
            }
            EntryChange::Modified { before, after } => (
                self.preview_file(&before.kind)?,
                self.preview_file(&after.kind)?,
            ),
        };
        match (before, after) {
            (PreviewFile::TooLarge, _) | (_, PreviewFile::TooLarge) => Ok(vec![format!(
                "Text diff omitted: a file exceeds the {MAX_DIFF_FILE_BYTES}-byte replay preview limit."
            )]),
            (PreviewFile::Bytes(before), PreviewFile::Bytes(after)) if before == after => Ok(vec![
                "File content is unchanged; only metadata differs.".to_owned(),
            ]),
            (PreviewFile::Bytes(before), PreviewFile::Bytes(after)) => {
                Ok(unified_text_preview(path, Some(&before), Some(&after)))
            }
            (PreviewFile::Bytes(before), PreviewFile::Absent) => {
                Ok(unified_text_preview(path, Some(&before), None))
            }
            (PreviewFile::Absent, PreviewFile::Bytes(after)) => {
                Ok(unified_text_preview(path, None, Some(&after)))
            }
            (PreviewFile::Absent, PreviewFile::Absent) => Ok(Vec::new()),
        }
    }

    fn preview_file(&self, kind: &SnapshotEntryKind) -> Result<PreviewFile> {
        let SnapshotEntryKind::File {
            object_id, size, ..
        } = kind
        else {
            return Ok(PreviewFile::Absent);
        };
        if *size > MAX_DIFF_FILE_BYTES {
            return Ok(PreviewFile::TooLarge);
        }
        Ok(PreviewFile::Bytes(
            self.store.load_object(*object_id, *size)?,
        ))
    }

    fn peek_next_offset(&mut self) -> Result<Option<u64>> {
        if let Some(event) = self.events.get(self.event_selected + 1) {
            return Ok(Some(event.monotonic_offset.as_nanoseconds()));
        }
        if !self.page_has_more {
            return Ok(None);
        }
        let Some(last) = self.events.last().map(|event| event.sequence) else {
            return Ok(None);
        };
        if self.pending_page.is_none() {
            self.pending_page = Some(self.store.load_timeline(
                self.run.id,
                Some(last),
                TIMELINE_PAGE_SIZE,
            )?);
        }
        let Some(page) = self.pending_page.as_ref() else {
            return Ok(None);
        };
        if page.events.is_empty() {
            self.page_has_more = false;
            self.pending_page = None;
            return Ok(None);
        }
        let offset = page.events[0].monotonic_offset.as_nanoseconds();
        Ok(Some(offset))
    }

    fn step_next_raw(&mut self) -> Result<bool> {
        if self.event_selected + 1 < self.events.len() {
            self.event_selected += 1;
            return Ok(true);
        }
        if !self.page_has_more {
            return Ok(false);
        }
        let Some(last) = self.events.last().map(|event| event.sequence) else {
            return Ok(false);
        };
        let page = match self.pending_page.take() {
            Some(page) => page,
            None => self
                .store
                .load_timeline(self.run.id, Some(last), TIMELINE_PAGE_SIZE)?,
        };
        if page.events.is_empty() {
            self.page_has_more = false;
            return Ok(false);
        }
        self.replace_page(page, 0);
        Ok(true)
    }

    fn step_previous_raw(&mut self) -> Result<bool> {
        if self.event_selected > 0 {
            self.event_selected -= 1;
            return Ok(true);
        }
        let Some(first) = self.events.first().map(|event| event.sequence.get()) else {
            return Ok(false);
        };
        if first == EventSequence::FIRST.get() {
            return Ok(false);
        }
        self.pending_page = None;
        let target = first - 1;
        let after = previous_page_cursor(target);
        let page =
            self.store
                .load_timeline(self.run.id, EventSequence::new(after), TIMELINE_PAGE_SIZE)?;
        let selected = page
            .events
            .iter()
            .position(|event| event.sequence.get() == target)
            .unwrap_or_else(|| page.events.len().saturating_sub(1));
        self.replace_page(page, selected);
        Ok(!self.events.is_empty())
    }

    fn jump_to(&mut self, target: EventSequence) -> Result<()> {
        self.pending_page = None;
        let half_page = u64::from(TIMELINE_PAGE_SIZE / 2);
        let after = target.get().saturating_sub(half_page + 1);
        let page =
            self.store
                .load_timeline(self.run.id, EventSequence::new(after), TIMELINE_PAGE_SIZE)?;
        let selected = page
            .events
            .iter()
            .position(|event| event.sequence == target)
            .unwrap_or(0);
        self.replace_page(page, selected);
        Ok(())
    }

    fn replace_page(&mut self, page: TimelinePage, selected: usize) {
        self.events = page.events;
        self.page_has_more = page.has_more;
        self.event_selected = selected.min(self.events.len().saturating_sub(1));
        self.ingest_current_page();
    }

    fn ingest_current_page(&mut self) {
        for event in &self.events {
            match &event.payload {
                EventPayload::ProcessObserved { process } => {
                    if self.processes.contains_key(&process.process_id)
                        || self.processes.len() < MAX_PROCESS_RECORDS
                    {
                        self.processes
                            .entry(process.process_id)
                            .and_modify(|record| record.process = process.clone())
                            .or_insert_with(|| ProcessRecord {
                                process: process.clone(),
                                first_sequence: event.sequence,
                                exit: None,
                            });
                    }
                }
                EventPayload::ProcessExited { process_id, status } => {
                    if let Some(record) = self.processes.get_mut(process_id) {
                        record.exit = Some((event.sequence, *status));
                    }
                }
                _ => {}
            }
        }
        self.process_selected = self
            .process_selected
            .min(self.visible_processes().len().saturating_sub(1));
    }

    fn after_event_move(&mut self) -> Result<()> {
        self.sync_checkpoint();
        self.sync_playback_target();
        self.refresh_terminal()
    }

    fn sync_checkpoint(&mut self) {
        let checkpoint = self.current_checkpoint().map(|value| value.id);
        if self.workspace.checkpoint() != checkpoint {
            self.workspace = WorkspaceState::Unloaded { checkpoint };
        }
    }

    fn sync_playback_target(&mut self) {
        self.playback_target_ns = self.selected_event().map_or(0, |event| {
            u128::from(event.monotonic_offset.as_nanoseconds())
        });
    }

    fn refresh_terminal(&mut self) -> Result<()> {
        let Some(selected) = self.events.get(..=self.event_selected) else {
            self.terminal = TerminalDocument::default();
            return Ok(());
        };
        let budget = self.terminal_cache.capacity().min(MAX_TERMINAL_VIEW_BYTES);
        let mut references = Vec::new();
        let mut total = 0_usize;
        let mut truncated = self
            .events
            .first()
            .is_some_and(|event| event.sequence != EventSequence::FIRST);

        // ponytail: this reconstructs a bounded page-local tail; use the
        // terminal_chunks index if exact screen state across pages is needed.
        for event in selected.iter().rev() {
            let EventPayload::TerminalOutput {
                object_id,
                byte_len,
                ..
            } = &event.payload
            else {
                continue;
            };
            let Ok(byte_len) = usize::try_from(*byte_len) else {
                truncated = true;
                break;
            };
            let Some(next) = total.checked_add(byte_len) else {
                truncated = true;
                break;
            };
            if next > budget {
                truncated = true;
                break;
            }
            total = next;
            references.push((*object_id, byte_len));
        }
        references.reverse();

        let mut raw = Vec::with_capacity(total);
        for (object_id, byte_len) in references {
            match self
                .terminal_cache
                .get_or_load(&self.store, object_id, byte_len)?
            {
                Some(bytes) => raw.extend_from_slice(bytes),
                None => truncated = true,
            }
        }
        self.terminal = TerminalDocument::parse(&raw, truncated);
        Ok(())
    }
}

fn previous_page_cursor(target: u64) -> u64 {
    target.saturating_sub(u64::from(TIMELINE_PAGE_SIZE))
}

fn build_run_rows(mut runs: Vec<Run>, selected: RunId) -> Vec<RunRow> {
    runs.sort_by_key(|run| (run.started_at, run.id));
    let parents = runs
        .iter()
        .map(|run| (run.id, run.parent.map(|parent| parent.run_id)))
        .collect::<BTreeMap<_, _>>();
    let rows = runs
        .into_iter()
        .map(|run| {
            let mut depth = 0;
            let mut cursor = run.parent.map(|parent| parent.run_id);
            let mut visited = BTreeSet::from([run.id]);
            while let Some(parent) = cursor {
                if depth == 32 || !visited.insert(parent) {
                    break;
                }
                depth += 1;
                cursor = parents.get(&parent).copied().flatten();
            }
            RunRow {
                run_id: run.id,
                depth,
                status: run.status,
                command: run.command,
                duration_ns: run
                    .monotonic_duration
                    .map(|duration| duration.as_nanoseconds()),
                parent: run
                    .parent
                    .map(|parent| (parent.run_id, parent.checkpoint_id)),
            }
        })
        .collect::<Vec<_>>();
    debug_assert!(rows.iter().any(|row| row.run_id == selected));
    rows
}

struct ObjectCache {
    capacity: usize,
    used: usize,
    entries: BTreeMap<ObjectId, Vec<u8>>,
    order: VecDeque<ObjectId>,
}

enum PreviewFile {
    Absent,
    TooLarge,
    Bytes(Vec<u8>),
}

fn unified_text_preview(path: &str, before: Option<&[u8]>, after: Option<&[u8]>) -> Vec<String> {
    let before_name = if before.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".to_owned()
    };
    let after_name = if after.is_some() {
        format!("b/{path}")
    } else {
        "/dev/null".to_owned()
    };
    let mut output = vec![format!("--- {before_name}"), format!("+++ {after_name}")];
    let before = before.unwrap_or_default();
    let after = after.unwrap_or_default();
    let (Ok(before), Ok(after)) = (std::str::from_utf8(before), std::str::from_utf8(after)) else {
        output.push("Binary or non-UTF-8 content diff omitted.".to_owned());
        return output;
    };
    let before = before.split_inclusive('\n').collect::<Vec<_>>();
    let after = after.split_inclusive('\n').collect::<Vec<_>>();
    if before.is_empty() && after.is_empty() {
        output.push("Empty file content.".to_owned());
        return output;
    }

    let common_prefix = before
        .iter()
        .zip(&after)
        .take_while(|(left, right)| left == right)
        .count();
    let common_suffix = before[common_prefix..]
        .iter()
        .rev()
        .zip(after[common_prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let context = 3;
    let start = common_prefix.saturating_sub(context);
    let before_changed_end = before.len().saturating_sub(common_suffix);
    let after_changed_end = after.len().saturating_sub(common_suffix);
    let suffix_context = common_suffix.min(context);
    let before_end = before_changed_end.saturating_add(suffix_context);
    let after_end = after_changed_end.saturating_add(suffix_context);
    output.push(format!(
        "@@ -{},{} +{},{} @@",
        hunk_start(start, before_end - start),
        before_end - start,
        hunk_start(start, after_end - start),
        after_end - start
    ));

    let mut truncated = false;
    for line in &before[start..common_prefix] {
        push_preview_line(&mut output, ' ', line, &mut truncated);
    }
    for line in &before[common_prefix..before_changed_end] {
        push_preview_line(&mut output, '-', line, &mut truncated);
    }
    for line in &after[common_prefix..after_changed_end] {
        push_preview_line(&mut output, '+', line, &mut truncated);
    }
    for line in &after[after_changed_end..after_end] {
        push_preview_line(&mut output, ' ', line, &mut truncated);
    }
    if truncated {
        output.push(format!(
            "… unified preview limited to {MAX_DIFF_LINES} lines"
        ));
    }
    output
}

const fn hunk_start(zero_based: usize, count: usize) -> usize {
    if count == 0 {
        zero_based
    } else {
        zero_based + 1
    }
}

fn push_preview_line(output: &mut Vec<String>, prefix: char, line: &str, truncated: &mut bool) {
    if output.len() >= MAX_DIFF_LINES.saturating_sub(1) {
        *truncated = true;
        return;
    }
    let content = line.strip_suffix('\n').unwrap_or(line).replace('\r', "␍");
    output.push(format!("{prefix}{content}"));
    if !line.ends_with('\n') {
        if output.len() >= MAX_DIFF_LINES.saturating_sub(1) {
            *truncated = true;
        } else {
            output.push("\\ No newline at end of file".to_owned());
        }
    }
}

impl ObjectCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: 0,
            entries: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    const fn capacity(&self) -> usize {
        self.capacity
    }

    fn get_or_load(
        &mut self,
        store: &Store,
        id: ObjectId,
        expected: usize,
    ) -> Result<Option<&[u8]>> {
        if self.entries.contains_key(&id) {
            self.touch(id);
            return Ok(self.entries.get(&id).map(Vec::as_slice));
        }
        if expected > self.capacity {
            return Ok(None);
        }
        let bytes = store.load_object(id, expected as u64)?;
        if bytes.len() != expected {
            return Err(ReplayError::TerminalLength {
                object_id: id,
                expected,
                actual: bytes.len(),
            });
        }
        self.insert(id, bytes);
        Ok(self.entries.get(&id).map(Vec::as_slice))
    }

    fn insert(&mut self, id: ObjectId, bytes: Vec<u8>) -> bool {
        if bytes.len() > self.capacity {
            return false;
        }
        if self.entries.contains_key(&id) {
            self.touch(id);
            return true;
        }
        while self.used.saturating_add(bytes.len()) > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.used -= removed.len();
            }
        }
        self.used += bytes.len();
        self.order.push_back(id);
        self.entries.insert(id, bytes);
        true
    }

    fn touch(&mut self, id: ObjectId) {
        // ponytail: O(n) LRU touch keeps this dependency-free; use an
        // intrusive map only if cache profiling shows the object count matters.
        if let Some(position) = self.order.iter().position(|candidate| *candidate == id) {
            self.order.remove(position);
        }
        self.order.push_back(id);
    }
}

pub(crate) fn event_name(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::RunStarted { .. } => "run started",
        EventPayload::WorkspaceIsolated { .. } => "workspace isolated",
        EventPayload::TerminalInput { .. } => "terminal input",
        EventPayload::TerminalInputRedacted { .. } => "input redacted",
        EventPayload::TerminalOutput { .. } => "terminal output",
        EventPayload::TerminalResized { .. } => "terminal resized",
        EventPayload::ProcessObserved { .. } => "process observed",
        EventPayload::ProcessExited { .. } => "process exited",
        EventPayload::FilesystemPathsDirtied { .. } => "paths dirtied",
        EventPayload::CheckpointStarted { .. } => "checkpoint started",
        EventPayload::CheckpointCommitted { .. } => "checkpoint committed",
        EventPayload::CheckpointFailed { .. } => "checkpoint failed",
        EventPayload::MarkerCreated { .. } => "marker created",
        EventPayload::RunInterrupted { .. } => "run interrupted",
        EventPayload::RunCompleted { .. } => "run completed",
        EventPayload::RecorderWarning { .. } => "recorder warning",
    }
}

pub(crate) fn event_detail(event: &Event) -> Vec<String> {
    let mut lines = vec![
        format!("sequence     {}", event.sequence),
        format!("wall ms      {}", event.wall_clock),
        format!(
            "offset        {}",
            format_duration(event.monotonic_offset.as_nanoseconds())
        ),
        format!("schema        {}", event.schema_version),
        format!("event id      {}", event.id),
    ];
    match &event.payload {
        EventPayload::RunStarted {
            root_process_id,
            terminal_stream_id,
        } => {
            lines.push(format!("root pid      {root_process_id}"));
            lines.push(format!("stream        {terminal_stream_id}"));
        }
        EventPayload::WorkspaceIsolated { strategy } => {
            lines.push(format!("clone         {strategy}"));
        }
        EventPayload::TerminalInput {
            object_id,
            byte_len,
            ..
        }
        | EventPayload::TerminalOutput {
            object_id,
            byte_len,
            ..
        } => {
            lines.push(format!("bytes         {byte_len}"));
            lines.push(format!("object        {}…", &object_id.to_string()[..12]));
        }
        EventPayload::TerminalInputRedacted {
            byte_len, reason, ..
        } => {
            lines.push(format!("bytes         {byte_len} (not stored)"));
            lines.push(format!("reason        {}", redaction_name(*reason)));
        }
        EventPayload::TerminalResized { columns, rows, .. } => {
            lines.push(format!("size          {columns}×{rows}"));
        }
        EventPayload::ProcessObserved { process } => {
            lines.push(format!("pid           {}", process.process_id));
            lines.push(format!("command       {}", process.command));
            if let Some(executable) = &process.executable {
                lines.push(format!("executable    {executable}"));
            }
        }
        EventPayload::ProcessExited { process_id, status } => {
            lines.push(format!("pid           {process_id}"));
            lines.push(format!("status        {}", format_exit(*status)));
        }
        EventPayload::FilesystemPathsDirtied { paths } => {
            lines.push(format!("paths         {} hint(s)", paths.len()));
            lines.extend(
                paths
                    .iter()
                    .take(8)
                    .map(|path| format!("              {}", path.as_str())),
            );
        }
        EventPayload::CheckpointStarted {
            checkpoint_id,
            reason,
        } => {
            lines.push(format!("checkpoint    {checkpoint_id}"));
            lines.push(format!("reason        {reason}"));
        }
        EventPayload::CheckpointCommitted {
            checkpoint_id,
            snapshot_id,
        } => {
            lines.push(format!("checkpoint    {checkpoint_id}"));
            lines.push(format!("snapshot      {}…", &snapshot_id.to_string()[..12]));
        }
        EventPayload::CheckpointFailed {
            checkpoint_id,
            failure,
        } => {
            lines.push(format!("checkpoint    {checkpoint_id}"));
            lines.push(format!("failure       {}", failure_kind(failure.kind)));
            lines.push(format!("              {}", failure.message));
        }
        EventPayload::MarkerCreated {
            checkpoint_id,
            label,
        } => {
            lines.push(format!("checkpoint    {checkpoint_id}"));
            lines.push(format!("label         {label}"));
        }
        EventPayload::RunInterrupted { signal } => {
            lines.push(format!("signal        {}", optional_signal(*signal)));
        }
        EventPayload::RunCompleted {
            status,
            exit_status,
        } => {
            lines.push(format!("run status    {status}"));
            lines.push(format!(
                "exit          {}",
                exit_status.map_or_else(|| "unavailable".to_owned(), format_exit)
            ));
        }
        EventPayload::RecorderWarning { warning } => {
            lines.push(format!("warning       {}", warning_code(warning.code)));
            lines.push(format!("              {}", warning.message));
        }
    }
    lines
}

pub(crate) fn format_duration(nanoseconds: u64) -> String {
    let seconds = nanoseconds / 1_000_000_000;
    let milliseconds = nanoseconds % 1_000_000_000 / 1_000_000;
    format!("{seconds}.{milliseconds:03}s")
}

pub(crate) fn format_exit(status: ProcessExitStatus) -> String {
    match status {
        ProcessExitStatus::Code(code) => format!("code {code}"),
        ProcessExitStatus::Signal(signal) => format!("signal {signal}"),
        ProcessExitStatus::Unknown => "unknown".to_owned(),
    }
}

pub(crate) fn entry_kind(kind: &SnapshotEntryKind) -> &'static str {
    match kind {
        SnapshotEntryKind::Directory => "directory",
        SnapshotEntryKind::File { .. } => "file",
        SnapshotEntryKind::Symlink { .. } => "symlink",
    }
}

fn optional_signal(signal: Option<i32>) -> String {
    signal.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

fn redaction_name(reason: InputRedactionReason) -> &'static str {
    match reason {
        InputRedactionReason::EchoDisabled => "echo disabled",
        InputRedactionReason::PolicyNever => "record-input=never",
        InputRedactionReason::EchoDetectionUnavailable => "echo detection unavailable",
        InputRedactionReason::ExportPolicy => "export policy",
    }
}

fn failure_kind(kind: RecorderFailureKind) -> &'static str {
    match kind {
        RecorderFailureKind::Snapshot => "snapshot",
        RecorderFailureKind::Storage => "storage",
        RecorderFailureKind::ConcurrentWorkspaceMutation => "workspace race",
        RecorderFailureKind::ResourceLimit => "resource limit",
        RecorderFailureKind::InternalInvariant => "internal invariant",
    }
}

fn warning_code(code: RecorderWarningCode) -> &'static str {
    match code {
        RecorderWarningCode::CloneFallback => "clone fallback",
        RecorderWarningCode::WatcherOverflow => "watcher overflow",
        RecorderWarningCode::ProcessObservationIncomplete => "process observation incomplete",
        RecorderWarningCode::InputEchoDetectionUncertain => "input echo uncertain",
        RecorderWarningCode::StorageLimit => "storage limit",
        RecorderWarningCode::FilesystemRace => "filesystem race",
        RecorderWarningCode::PrivacyCleanupFailed => "privacy cleanup failed",
        RecorderWarningCode::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_cache_evicts_oldest_without_exceeding_capacity() {
        let first = ObjectId::digest(b"first");
        let second = ObjectId::digest(b"second");
        let third = ObjectId::digest(b"third");
        let mut cache = ObjectCache::new(5);
        assert!(cache.insert(first, vec![1, 2, 3]));
        assert!(cache.insert(second, vec![4, 5]));
        assert!(cache.insert(third, vec![6, 7, 8]));
        assert!(!cache.entries.contains_key(&first));
        assert!(cache.entries.contains_key(&second));
        assert!(cache.entries.contains_key(&third));
        assert_eq!(cache.used, 5);
        assert!(!cache.insert(ObjectId::digest(b"large"), vec![0; 6]));
    }

    #[test]
    fn backward_page_cursor_places_target_at_page_end() {
        assert_eq!(previous_page_cursor(256), 0);
        assert_eq!(previous_page_cursor(512), 256);
        assert_eq!(previous_page_cursor(7), 0);
    }

    #[test]
    fn unified_preview_is_bounded_and_keeps_context() {
        let preview = unified_text_preview(
            "file.txt",
            Some(b"same\nbefore\ntail\n"),
            Some(b"same\nafter\ntail\n"),
        );
        assert_eq!(preview[0], "--- a/file.txt");
        assert_eq!(preview[1], "+++ b/file.txt");
        assert!(preview.iter().any(|line| line == " same"));
        assert!(preview.iter().any(|line| line == "-before"));
        assert!(preview.iter().any(|line| line == "+after"));
        assert!(preview.iter().any(|line| line == " tail"));

        let line_endings = unified_text_preview("line.txt", Some(b"value\r\n"), Some(b"value\n"));
        assert!(line_endings.iter().any(|line| line == "-value␍"));
        assert!(line_endings.iter().any(|line| line == "+value"));
        assert!(!line_endings.iter().any(|line| line.contains('\r')));

        let many = "line\n".repeat(MAX_DIFF_LINES * 2);
        let preview = unified_text_preview("large.txt", None, Some(many.as_bytes()));
        assert!(preview.len() <= MAX_DIFF_LINES);
        assert!(preview.last().unwrap().contains("limited"));
    }
}
