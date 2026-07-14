use std::path::Path;

use rewind_domain::ObjectId;
use rewind_store::{MAX_OBJECT_PAGE, ObjectIdPage, Store, StoreError};
use serde::Serialize;

const REPORT_SAMPLE_LIMIT: usize = 1_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct GcReport {
    pub(crate) dry_run: bool,
    pub(crate) unreachable_objects: u64,
    pub(crate) reclaimable_bytes: u64,
    pub(crate) deleted_objects: u64,
    pub(crate) reclaimed_bytes: u64,
    pub(crate) sample: Vec<ObjectId>,
    pub(crate) sample_truncated: bool,
    pub(crate) crash_artifact_files: u64,
    pub(crate) crash_artifact_bytes: u64,
    pub(crate) deleted_crash_artifact_files: u64,
    pub(crate) reclaimed_crash_artifact_bytes: u64,
    pub(crate) crash_artifact_sample: Vec<String>,
    pub(crate) crash_artifact_sample_truncated: bool,
}

pub(crate) fn collect(store_root: &Path, delete: bool) -> Result<GcReport, StoreError> {
    let mut store = Store::open(store_root)?;
    store.validate_gc_references()?;
    let mut references = ReferenceCursor::new(&store)?;
    let mut object_cursor = None;
    let mut report = GcReport {
        dry_run: !delete,
        unreachable_objects: 0,
        reclaimable_bytes: 0,
        deleted_objects: 0,
        reclaimed_bytes: 0,
        sample: Vec::new(),
        sample_truncated: false,
        crash_artifact_files: 0,
        crash_artifact_bytes: 0,
        deleted_crash_artifact_files: 0,
        reclaimed_crash_artifact_bytes: 0,
        crash_artifact_sample: Vec::new(),
        crash_artifact_sample_truncated: false,
    };
    loop {
        let page = store.list_objects(object_cursor, MAX_OBJECT_PAGE)?;
        for object in &page.objects {
            references.advance_before(&store, object.id)?;
            if references.current() == Some(object.id) {
                object_cursor = Some(object.id);
                continue;
            }
            let physical = object.physical_size.unwrap_or(0);
            report.unreachable_objects = report.unreachable_objects.saturating_add(1);
            report.reclaimable_bytes = report.reclaimable_bytes.saturating_add(physical);
            if report.sample.len() < REPORT_SAMPLE_LIMIT {
                report.sample.push(object.id);
            } else {
                report.sample_truncated = true;
            }
            if delete {
                store.delete_object(object.id)?;
                report.deleted_objects = report.deleted_objects.saturating_add(1);
                report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(physical);
            }
            object_cursor = Some(object.id);
        }
        if !page.has_more {
            break;
        }
    }
    let mut orphan_cursor = None;
    loop {
        let page = store.list_physical_orphans(orphan_cursor.as_deref(), MAX_OBJECT_PAGE)?;
        for orphan in &page.files {
            report.crash_artifact_files = report.crash_artifact_files.saturating_add(1);
            report.crash_artifact_bytes = report
                .crash_artifact_bytes
                .saturating_add(orphan.stored_size);
            if report.crash_artifact_sample.len() < REPORT_SAMPLE_LIMIT {
                report
                    .crash_artifact_sample
                    .push(orphan.relative_path.clone());
            } else {
                report.crash_artifact_sample_truncated = true;
            }
            if delete {
                let deleted = store.delete_physical_orphan(orphan)?;
                report.deleted_crash_artifact_files =
                    report.deleted_crash_artifact_files.saturating_add(1);
                report.reclaimed_crash_artifact_bytes = report
                    .reclaimed_crash_artifact_bytes
                    .saturating_add(deleted.stored_size);
            }
            orphan_cursor = Some(orphan.relative_path.clone());
        }
        if !page.has_more {
            break;
        }
    }
    Ok(report)
}

struct ReferenceCursor {
    page: ObjectIdPage,
    index: usize,
    after: Option<ObjectId>,
}

impl ReferenceCursor {
    fn new(store: &Store) -> Result<Self, StoreError> {
        Ok(Self {
            page: store.list_referenced_object_ids(None, MAX_OBJECT_PAGE)?,
            index: 0,
            after: None,
        })
    }

    fn current(&self) -> Option<ObjectId> {
        self.page.ids.get(self.index).copied()
    }

    fn advance_before(&mut self, store: &Store, target: ObjectId) -> Result<(), StoreError> {
        loop {
            while self.current().is_some_and(|id| id < target) {
                self.after = self.current();
                self.index += 1;
            }
            if self.current().is_some() || !self.page.has_more {
                return Ok(());
            }
            self.page = store.list_referenced_object_ids(self.after, MAX_OBJECT_PAGE)?;
            self.index = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn gc_reports_and_removes_unindexed_objects_and_temporary_files() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let payload = b"published before metadata";
        let id = ObjectId::digest(payload);
        let object_path = store.objects().path_for(id);
        let parent = object_path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        let mut envelope = Vec::from(b"RWOB\x01\x00\x00\x00".as_slice());
        envelope.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        envelope.extend_from_slice(payload);
        fs::write(&object_path, &envelope).unwrap();
        let temporary = parent.join(".tmp-999-1");
        fs::write(&temporary, b"partial").unwrap();
        drop(store);

        let dry_run = collect(temp.path(), false).unwrap();
        assert_eq!(dry_run.crash_artifact_files, 2);
        assert_eq!(dry_run.deleted_crash_artifact_files, 0);
        assert!(object_path.exists());
        assert!(temporary.exists());

        let deleted = collect(temp.path(), true).unwrap();
        assert_eq!(deleted.deleted_crash_artifact_files, 2);
        assert_eq!(
            deleted.reclaimed_crash_artifact_bytes,
            envelope.len() as u64 + 7
        );
        assert!(!object_path.exists());
        assert!(!temporary.exists());
    }
}
