use std::cmp::Ordering;

use rewind_domain::{Snapshot, SnapshotEntry, SnapshotId, SnapshotPath};

/// One complete, reversible workspace-entry change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EntryChange {
    /// A path exists only in the later snapshot.
    Added {
        /// Complete later entry.
        entry: SnapshotEntry,
    },
    /// A path exists only in the earlier snapshot.
    Removed {
        /// Complete earlier entry.
        entry: SnapshotEntry,
    },
    /// A path exists in both snapshots but differs in kind, content, or mode.
    Modified {
        /// Complete earlier entry.
        before: SnapshotEntry,
        /// Complete later entry.
        after: SnapshotEntry,
    },
}

impl EntryChange {
    /// Returns the canonical path affected by this change.
    #[must_use]
    pub fn path(&self) -> &SnapshotPath {
        match self {
            Self::Added { entry } | Self::Removed { entry } => &entry.path,
            Self::Modified { before, .. } => &before.path,
        }
    }

    fn reverse(&self) -> Self {
        match self {
            Self::Added { entry } => Self::Removed {
                entry: entry.clone(),
            },
            Self::Removed { entry } => Self::Added {
                entry: entry.clone(),
            },
            Self::Modified { before, after } => Self::Modified {
                before: after.clone(),
                after: before.clone(),
            },
        }
    }
}

/// Sorted, lossless differences between two canonical snapshots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotDiff {
    /// Earlier snapshot identity.
    pub from: SnapshotId,
    /// Later snapshot identity.
    pub to: SnapshotId,
    /// Every added, removed, or modified entry in canonical path order.
    pub changes: Vec<EntryChange>,
}

impl SnapshotDiff {
    /// Returns the same evidence viewed in the opposite direction.
    #[must_use]
    pub fn reversed(&self) -> Self {
        Self {
            from: self.to,
            to: self.from,
            changes: self.changes.iter().map(EntryChange::reverse).collect(),
        }
    }
}

/// Compares all supported entry state using a linear merge of canonical trees.
#[must_use]
pub fn diff_snapshots(before: &Snapshot, after: &Snapshot) -> SnapshotDiff {
    let left = before.manifest.entries();
    let right = after.manifest.entries();
    let (mut left_index, mut right_index) = (0, 0);
    let mut changes = Vec::new();

    while left_index < left.len() || right_index < right.len() {
        match (left.get(left_index), right.get(right_index)) {
            (Some(before_entry), Some(after_entry)) => {
                match before_entry.path.cmp(&after_entry.path) {
                    Ordering::Less => {
                        changes.push(EntryChange::Removed {
                            entry: before_entry.clone(),
                        });
                        left_index += 1;
                    }
                    Ordering::Greater => {
                        changes.push(EntryChange::Added {
                            entry: after_entry.clone(),
                        });
                        right_index += 1;
                    }
                    Ordering::Equal => {
                        if before_entry != after_entry {
                            changes.push(EntryChange::Modified {
                                before: before_entry.clone(),
                                after: after_entry.clone(),
                            });
                        }
                        left_index += 1;
                        right_index += 1;
                    }
                }
            }
            (Some(entry), None) => {
                changes.push(EntryChange::Removed {
                    entry: entry.clone(),
                });
                left_index += 1;
            }
            (None, Some(entry)) => {
                changes.push(EntryChange::Added {
                    entry: entry.clone(),
                });
                right_index += 1;
            }
            (None, None) => break,
        }
    }

    SnapshotDiff {
        from: before.id,
        to: after.id,
        changes,
    }
}
