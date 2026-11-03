//! Snapshots, undo/redo and session diffs.
use super::*;

fn describe_undo(r: &Result<UndoResult, StoreError>) -> String {
    match r {
        Ok(UndoResult::Done(rep)) => format!("done: {} message(s) hidden, files restored={} (restored {}, deleted {})", rep.hidden_messages, rep.files_restored, rep.restored.len(), rep.deleted.len()),
        Ok(other) => format!("{other:?}"),
        Err(e) => format!("store error: {e}"),
    }
}

/// Also reported to the user through `UndoReport::skipped`; the log keeps the file names.
fn log_skipped(sid: &str, skipped: &[crate::snapshot::Skipped]) {
    if !skipped.is_empty() {
        let list: Vec<String> = skipped.iter().map(|s| format!("{} ({})", s.path, s.reason)).collect();
        crate::log_warn!("engine", "undo session={sid}: {} file(s) not covered by the snapshot were not restored: {}", skipped.len(), list.join(", "));
    }
}

impl Engine {
    /// Capture the working tree before a step. Best effort: a failure disables snapshots for the session with one notice.
    pub(super) fn take_snapshot(&self, session: &Session, ui: &dyn Ui) -> Option<String> {
        if !self.cfg.snapshots || self.snapshot_off.lock().unwrap().contains(&session.id) {
            return None;
        }
        let prev = {
            let st = self.store();
            st.last_snapshot(&session.id).ok().flatten().or_else(|| session.parent_id.as_ref().and_then(|p| st.last_snapshot(p).ok().flatten()))
        };
        let started = std::time::Instant::now();
        let snaps = self.snaps(session);
        match snaps.capture(std::path::Path::new(&session.directory), prev.as_deref()) {
            Ok(id) => {
                if crate::log::enabled(crate::log::Level::Debug, "snapshot") {
                    let files = snaps.manifest(&id).map(|m| m.files.len().to_string()).unwrap_or_else(|_| "?".into());
                    crate::log_debug!("snapshot", "captured session={} files={files} ms={}", session.id, started.elapsed().as_millis());
                }
                Some(id)
            }
            Err(e) => {
                crate::log_warn!("snapshot", "capture failed session={} after {} ms, snapshots are off for this session: {e}", session.id, started.elapsed().as_millis());
                self.snapshot_off.lock().unwrap().insert(session.id.clone());
                ui.event(&UiEvent::Notice(&format!("snapshots are off for this session (undo and diff unavailable): {e}")));
                None
            }
        }
    }

    pub(super) fn snaps(&self, session: &Session) -> Snapshots {
        let mut s = Snapshots::new(self.cfg.app_dir.join("data/snapshots").join(&session.project_id));
        s.max_files = self.cfg.snapshot_max_files;
        s
    }

    /// Remove old snapshot data that no recent session refers to. Returns (manifests, blobs) removed.
    pub fn prune_snapshots(&self, project_ids: &[String], max_age: Duration) -> (usize, usize) {
        self.prune_snapshots_protecting(project_ids, max_age, &[])
    }

    /// Like `prune_snapshots`, and also keeps every snapshot of the `protect` sessions (ones being resumed), however old.
    pub fn prune_snapshots_protecting(&self, project_ids: &[String], max_age: Duration, protect: &[String]) -> (usize, usize) {
        let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0) - max_age.as_millis() as i64;
        let mut total = (0, 0);
        for pid in project_ids {
            let mut keep = self.store().live_snapshots(pid, since).unwrap_or_default();
            for sid in protect {
                if let Ok(msgs) = self.store().messages(sid) {
                    keep.extend(msgs.iter().filter(|m| m.kind == "assistant").filter_map(|m| m.data["snapshot"].as_str().map(String::from)));
                }
            }
            let (m, b) = Snapshots::new(self.cfg.app_dir.join("data/snapshots").join(pid)).prune(&keep, max_age);
            total = (total.0 + m, total.1 + b);
        }
        total
    }

    /// Undo the latest turn (user message and everything after): files go back to how they were before the turn's
    /// first step, the messages are hidden. Repeat to go further back. `redo` reverses it; the next prompt commits it.
    pub fn undo(&self, sid: &str) -> Result<UndoResult, StoreError> {
        if !self.store().claim(sid)? {
            return Ok(UndoResult::Busy);
        }
        let r = self.undo_inner(sid);
        self.store().release(sid)?;
        crate::log_info!("engine", "undo session={sid}: {}", describe_undo(&r));
        r
    }

    fn undo_inner(&self, sid: &str) -> Result<UndoResult, StoreError> {
        let (session, msgs, revert) = {
            let st = self.store();
            (st.session(sid)?.ok_or(StoreError::SessionGone)?, st.messages(sid)?, st.revert(sid)?)
        };
        let marker = revert.as_ref().and_then(|r| r["messageId"].as_str()).map(String::from);
        let Some(turn) = msgs.iter().rfind(|m| m.kind == "user" && marker.as_deref().is_none_or(|mk| m.id.as_str() < mk)) else {
            return Ok(UndoResult::NothingToUndo);
        };
        let snapshot = msgs.iter().filter(|m| m.kind == "assistant" && m.id > turn.id).find_map(|m| m.data["snapshot"].as_str().map(String::from));
        let mut redo = revert.as_ref().and_then(|r| r["redo"].as_str()).map(String::from);
        let mut report = UndoReport { hidden_messages: msgs.iter().filter(|m| m.id >= turn.id && (m.kind == "user" || m.kind == "assistant")).count(), ..Default::default() };
        if let (true, Some(snap)) = (self.cfg.snapshots, snapshot) {
            let hint = self.store().last_snapshot(sid)?;
            match self.snaps(&session).restore(std::path::Path::new(&session.directory), &snap, hint.as_deref()) {
                Ok(r) => {
                    log_skipped(sid, &r.skipped);
                    report.restored = r.restored;
                    report.deleted = r.deleted;
                    report.skipped = r.skipped;
                    report.files_restored = true;
                    redo = redo.or(Some(r.before)); // the state before the first undo is what redo returns to
                }
                Err(e) => return Ok(UndoResult::Failed(e.to_string())),
            }
        }
        self.store().set_revert(sid, Some(&json!({ "messageId": turn.id, "redo": redo })))?;
        Ok(UndoResult::Done(report))
    }

    /// Reverse staged undo(s): files and messages come back as they were before the first `undo`.
    pub fn redo(&self, sid: &str) -> Result<UndoResult, StoreError> {
        if !self.store().claim(sid)? {
            return Ok(UndoResult::Busy);
        }
        let r = (|| {
            let (session, revert) = {
                let st = self.store();
                (st.session(sid)?.ok_or(StoreError::SessionGone)?, st.revert(sid)?)
            };
            let Some(revert) = revert else { return Ok(UndoResult::NothingToUndo) };
            let mut report = UndoReport::default();
            if let (true, Some(redo)) = (self.cfg.snapshots, revert["redo"].as_str()) {
                match self.snaps(&session).restore(std::path::Path::new(&session.directory), redo, None) {
                    Ok(r) => {
                        log_skipped(sid, &r.skipped);
                        report.restored = r.restored;
                        report.deleted = r.deleted;
                        report.skipped = r.skipped;
                        report.files_restored = true;
                    }
                    Err(e) => return Ok(UndoResult::Failed(e.to_string())),
                }
            }
            self.store().set_revert(sid, None)?;
            Ok(UndoResult::Done(report))
        })();
        self.store().release(sid)?;
        crate::log_info!("engine", "redo session={sid}: {}", describe_undo(&r));
        r
    }

    /// Make a staged undo permanent: the hidden messages are deleted.
    pub fn commit_pending(&self, sid: &str) -> Result<usize, StoreError> {
        let st = self.store();
        let Some(revert) = st.revert(sid)? else { return Ok(0) };
        let n = match revert["messageId"].as_str() {
            Some(id) => st.delete_messages_from(sid, id)?,
            None => 0,
        };
        st.set_revert(sid, None)?;
        Ok(n)
    }

    /// Changes in the working tree since the session's first step.
    pub fn session_diff(&self, sid: &str) -> Result<DiffResult, StoreError> {
        let (session, msgs, last) = {
            let st = self.store();
            (st.session(sid)?.ok_or(StoreError::SessionGone)?, st.messages(sid)?, st.last_snapshot(sid)?)
        };
        let Some(first) = msgs.iter().filter(|m| m.kind == "assistant").find_map(|m| m.data["snapshot"].as_str().map(String::from)) else {
            return Ok(DiffResult::NoSnapshots);
        };
        let snaps = self.snaps(&session);
        let now = match snaps.capture(std::path::Path::new(&session.directory), last.as_deref()) {
            Ok(id) => id,
            Err(e) => return Ok(DiffResult::Failed(e.to_string())),
        };
        Ok(snaps.diff(&first, &now).map_or_else(|e| DiffResult::Failed(e.to_string()), DiffResult::Files))
    }
}
