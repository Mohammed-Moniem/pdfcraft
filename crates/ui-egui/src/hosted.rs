//! Acknowledged embedding saves operate on the actual UI document and its exact generation.
use std::cell::Cell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use crate::{PdfCraftApp, SaveTarget};
use pdfcraft_engine::DocId;

pub type HostSaveFn = Box<dyn Fn(&str, &[u8], bool, DocId, u64) -> Result<u64, String>>;
type AfterSave = Box<dyn FnOnce(&mut PdfCraftApp) + Send>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    Failed,
}
#[derive(Default)]
struct QueueState {
    next: u64,
    pending: BTreeSet<u64>,
    finished: Vec<(u64, SaveOutcome)>,
}
#[derive(Clone, Default)]
pub struct SaveQueue(Rc<Cell<QueueState>>);
/// Monotonic protocol revision of the visible engine document. Dirty/pending storage state is
/// reported alongside it, never counted as an edit of the bytes being saved.
#[derive(Default)]
pub struct RevisionClock {
    current: Option<(u64, u64)>,
    revision: u64,
}
impl RevisionClock {
    pub fn sync(&mut self, document: u64, engine_revision: u64) -> u64 {
        let current = (document, engine_revision);
        if self.current != Some(current) {
            self.revision = match self.current {
                None => engine_revision,
                Some((old_document, old_revision)) if old_document == document && engine_revision > old_revision => {
                    self.revision.saturating_add(engine_revision - old_revision)
                }
                Some(_) => self.revision.saturating_add(1),
            };
            self.current = Some(current);
        }
        self.revision
    }
}
impl SaveQueue {
    fn with<T>(&self, f: impl FnOnce(&mut QueueState) -> T) -> T {
        let mut state = self.0.take();
        let result = f(&mut state);
        self.0.set(state);
        result
    }
    pub fn start(&self) -> Result<u64, String> {
        self.with(|s| {
            if s.pending.len() + s.finished.len() >= 8 || s.next >= 10000 {
                return Err("host save ticket limit reached".into());
            }
            s.next += 1;
            s.pending.insert(s.next);
            Ok(s.next)
        })
    }
    pub fn finish(&self, ticket: u64, outcome: SaveOutcome) {
        self.with(|s| {
            if s.pending.remove(&ticket) {
                s.finished.push((ticket, outcome));
            }
        });
    }
    fn take_finished(&self) -> Vec<(u64, SaveOutcome)> {
        self.with(|s| std::mem::take(&mut s.finished))
    }
}
pub(crate) struct PendingSave {
    pub document: DocId,
    ticket: u64,
    generation: u64,
    bytes: Arc<Vec<u8>>,
    // Completed tickets stay in their bounded owner group until every overlapping write has
    // settled. In particular an early Save Changes ACK must not drop its Close continuation.
    outcome: Option<SaveOutcome>,
    failed_overlap: bool,
    after: Option<AfterSave>,
}

impl PdfCraftApp {
    /// All documents owned by the frame, including parked human UI tabs and uncertain writes.
    pub fn host_has_unsaved_documents(&self) -> bool {
        self.first_dirty().is_some()
            || self.session.docs().iter().any(|d| d.dirty)
            || !self.pending_host_saves.is_empty()
            || !self.host_unsaved.is_empty()
    }
    pub fn host_pending_saves(&self) -> usize {
        self.pending_host_saves.len()
    }
    pub fn host_has_drafts(&self) -> bool {
        (0..self.views.len()).any(|i| self.has_unsaved_drafts(i))
    }
    pub fn host_interaction_busy(&self) -> bool {
        self.modal_open()
    }
    pub fn host_document_identity(&self) -> Option<(DocId, u64)> {
        self.active_ids().and_then(|(_, id)| self.session.get(id).map(|d| (id, d.edit_generation())))
    }
    /// Programmatic host opens never park an unsaved owner or an unacknowledged write.
    pub fn open_from_host(&mut self, name: &str, bytes: Vec<u8>) -> Result<DocId, String> {
        if bytes.len() > 32 << 20 {
            return Err("host document exceeds 32 MiB".into());
        }
        if self.host_has_unsaved_documents() || self.host_interaction_busy() {
            return Err("save or close unsaved documents and finish pending writes before opening".into());
        }
        // Prepare first: password-protected/invalid input cannot leave a deferred password open
        // that later replaces the host's confirmed document without a successful request.
        let bytes = Arc::new(bytes);
        let prepared = self.session.prepare_open(name, None, bytes.clone(), None).map_err(|e| e.to_string())?;
        self.finish_host_open(name, bytes.len(), prepared)
    }
    pub fn close_from_host(&mut self, discard: bool) -> Result<(), String> {
        if !self.pending_host_saves.is_empty() || self.host_interaction_busy() {
            return Err("a pending save or document interaction still owns this frame".into());
        }
        let active = self.active_ids().map(|(_, id)| id);
        if self.views.iter().enumerate().any(|(i, v)| Some(v.id) != active && self.has_unsaved_work(i))
            || self.session.docs().iter().any(|d| Some(d.id) != active && (d.dirty || self.host_unsaved.contains(&d.id)))
        {
            return Err("another owned document contains unsaved work".into());
        }
        if self.host_has_unsaved_documents() && !discard {
            return Err("save or explicitly discard the active document".into());
        }
        if let Some((index, _)) = self.active_ids() {
            self.close_tab(index);
        }
        Ok(())
    }
    pub(crate) fn save_host_then(&mut self, id: DocId, target: SaveTarget, after: AfterSave) -> bool {
        if self.pending_host_saves.len() >= 8 {
            self.notify_tr("Too many pending host saves.");
            return false;
        }
        if !self.flatten_fill_sign_for_save(id) {
            return false;
        }
        let Some(doc) = self.session.get(id) else { return false };
        let name = doc.name.clone();
        let generation = doc.edit_generation();
        let bytes = match self.session.save_bytes(id).and_then(|bytes| self.session.check_save(id, &bytes).map(|()| bytes)) {
            Ok(bytes)
                if bytes.len() <= 32 << 20
                    && self.pending_host_saves.iter().map(|save| save.bytes.len()).sum::<usize>().saturating_add(bytes.len()) <= 64 << 20 =>
            {
                bytes
            }
            _ => {
                self.notify_tr("Could not prepare the bounded host save.");
                return false;
            }
        };
        let Some(save) = &self.host_save_versioned else { return false };
        match save(&name, &bytes, target == SaveTarget::As, id, generation) {
            Ok(ticket) if !self.pending_host_saves.iter().any(|save| save.ticket == ticket) => {
                self.host_unsaved.insert(id);
                self.pending_host_saves.push(PendingSave {
                    document: id,
                    ticket,
                    generation,
                    bytes,
                    outcome: None,
                    failed_overlap: false,
                    after: Some(after),
                });
            }
            _ => {
                for pending in self.pending_host_saves.iter_mut().filter(|pending| pending.document == id) {
                    pending.failed_overlap = true;
                }
                self.host_unsaved.insert(id);
                self.notify_tr("Host save failed; the document remains unsaved.");
            }
        }
        self.sync_unsaved_flag();
        false // The ordinary UI Save/Save Changes path must wait for host acknowledgement.
    }
    pub fn poll_host_saves(&mut self) {
        for (ticket, outcome) in self.host_saves.take_finished() {
            if let Some(save) = self.pending_host_saves.iter_mut().find(|save| save.ticket == ticket) {
                save.outcome = Some(outcome);
            }
        }
        let mut owners = Vec::new();
        for save in &self.pending_host_saves {
            if !owners.contains(&save.document) {
                owners.push(save.document);
            }
        }
        for document in owners {
            if self.pending_host_saves.iter().any(|save| save.document == document && save.outcome.is_none()) {
                continue;
            }
            let mut group = Vec::new();
            let mut index = 0;
            while index < self.pending_host_saves.len() {
                if self.pending_host_saves[index].document == document {
                    group.push(self.pending_host_saves.remove(index));
                } else {
                    index += 1;
                }
            }
            let latest = group.last().expect("an owner group is nonempty");
            let view = self.views.iter().position(|v| v.id == document);
            let current = view.is_some_and(|i| !self.has_unsaved_drafts(i))
                && self.session.get(document).is_some_and(|doc| latest.generation == doc.edit_generation());
            if !current || latest.failed_overlap || latest.outcome != Some(SaveOutcome::Saved) {
                self.notify_tr("Save was not confirmed for the current document state; unsaved work is retained.");
                continue;
            }
            // A successful current retry may clean the source even when it started before an
            // earlier failure was polled. That earlier failure still vetoes deferred Close.
            let can_continue = !self.host_interaction_busy()
                && group.iter().all(|save| save.generation == latest.generation && !save.failed_overlap && save.outcome == Some(SaveOutcome::Saved));
            // Adopt only after the last ACK; rebasing earlier would stale other snapshots.
            let bytes = latest.bytes.clone();
            if self.session.mark_saved(document, bytes, None).is_err() {
                self.notify_tr("Stored bytes could not be adopted; the document remains unsaved.");
                continue;
            }
            if let Some(doc) = self.session.get(document)
                && let Some(view) = self.views.iter_mut().find(|view| view.id == document)
            {
                view.document_changed(&doc.info);
            }
            self.host_unsaved.remove(&document);
            if can_continue {
                for save in group {
                    if let Some(after) = save.after {
                        after(self);
                    }
                }
            }
        }
        self.sync_unsaved_flag();
    }

    /// A later explicit Cancel withdraws any earlier deferred Close/All/Quit action, while
    /// successful writes may still settle the saved snapshot. It cannot cancel host storage.
    pub(crate) fn cancel_host_save_continuations(&mut self, document: Option<DocId>) {
        for save in &mut self.pending_host_saves {
            if document.is_none_or(|document| document == save.document) {
                save.after = None;
            }
        }
    }

    pub fn host_commands(&self, query: &str) -> serde_json::Value {
        let query = query.to_ascii_lowercase();
        let doc = self.active_ids().and_then(|(_, id)| self.session.get(id));
        serde_json::json!(
            HOST_COMMANDS
                .iter()
                .filter(|(id, label)| id.contains(&query) || label.to_ascii_lowercase().contains(&query))
                .map(|(id, label)| {
                    let enabled = !self.host_has_drafts()
                        && !self.host_interaction_busy()
                        && doc.is_some_and(|doc| match *id {
                            "edit.undo" => doc.can_undo().is_some(),
                            "edit.redo" => doc.can_redo().is_some(),
                            "pages.rotate" => doc.editable() && doc.allows_assembly(),
                            _ => doc.editable() && doc.allows_modification(),
                        });
                    serde_json::json!({"id":id,"label":label,"enabled":enabled})
                })
                .collect::<Vec<_>>()
        )
    }
    pub fn host_inspect(&self) -> Result<serde_json::Value, String> {
        let (_, id) = self.active_ids().ok_or("no document is open")?;
        let doc = self.session.get(id).ok_or("no document is open")?;
        let metadata = ["Title", "Author", "Subject", "Keywords"]
            .map(|key| (key, doc.info_value(key).unwrap_or_default().chars().take(4096).collect::<String>()))
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        Ok(serde_json::json!({"documentId":id.0,"engineRevision":doc.edit_generation(),"name":doc.name,"dirty":self.host_has_unsaved_documents(),
            "activeDirty":self.active.is_some_and(|i| self.has_unsaved_work(i)),"pendingWrites":self.host_pending_saves(),"pendingDrafts":self.host_has_drafts(),
            "ownedDocuments":self.session.docs().len(),"pageCount":doc.info.pages.len(),"pages":doc.info.pages.iter().take(128).map(|p| serde_json::json!({"width":p.width,"height":p.height,"rotation":p.rotation})).collect::<Vec<_>>(),
            "metadata":metadata }))
    }
    pub fn host_export(&self) -> Result<Vec<u8>, String> {
        if self.host_has_drafts() || self.host_interaction_busy() {
            return Err("apply or cancel unfinished UI typing before exporting".into());
        }
        let (_, id) = self.active_ids().ok_or("no document is open")?;
        let bytes = self.session.save_bytes(id).map_err(|e| e.to_string())?;
        if bytes.len() > 32 << 20 {
            return Err("export exceeds 32 MiB".into());
        }
        self.session.check_save(id, &bytes).map_err(|e| e.to_string())?;
        Ok(bytes.as_ref().clone())
    }
    /// A deliberately bounded subset of real engine edits, always reflected in the visible UI.
    pub fn host_execute(&mut self, command: &str, params: &serde_json::Value) -> Result<serde_json::Value, String> {
        use pdfcraft_engine::{AddedText, Edit};
        use serde_json::{Value, json};
        if !HOST_COMMANDS.iter().any(|(id, _)| *id == command) {
            return Err("unsupported host command".into());
        }
        if !params.is_object() || self.host_has_drafts() || self.host_interaction_busy() {
            return Err("apply or cancel unfinished UI typing before a host edit".into());
        }
        let (_, id) = self.active_ids().ok_or("no document is open")?;
        let doc = self.session.get(id).ok_or("no document is open")?;
        let before = doc.edit_generation();
        let page_count = doc.info.pages.len();
        let index = |key: &str| -> Result<usize, String> {
            let n = params.get(key).and_then(Value::as_u64).ok_or("a non-negative page index is required")?;
            usize::try_from(n).map_err(|_| "page index is too large".into())
        };
        let number = |key: &str, default: f64| -> Result<f64, String> {
            let n = params.get(key).map_or(Some(default), Value::as_f64).ok_or("a finite bounded number is required")?;
            if n.is_finite() && n.abs() <= 1_000_000.0 { Ok(n) } else { Err("number exceeds the host limit".into()) }
        };
        let edit = match command {
            "document.setInfo" => {
                let key = params.get("key").and_then(Value::as_str).ok_or("metadata key is required")?;
                let value = params.get("value").and_then(Value::as_str).ok_or("metadata value is required")?;
                if !["Title", "Author", "Subject", "Keywords"].contains(&key) || value.len() > 4096 {
                    return Err("unsupported metadata key or oversized value".into());
                }
                Some(Edit::SetInfo { key: key.into(), value: value.into() })
            }
            "pages.rotate" => {
                let pages = params.get("pages").and_then(Value::as_array).ok_or("pages is required")?;
                if pages.is_empty() || pages.len() > 128 {
                    return Err("choose between 1 and 128 pages".into());
                }
                let pages = pages
                    .iter()
                    .map(|p| {
                        p.as_u64()
                            .and_then(|p| usize::try_from(p).ok())
                            .filter(|p| *p < page_count)
                            .ok_or("page index outside the document".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if pages.iter().copied().collect::<BTreeSet<_>>().len() != pages.len() {
                    return Err("duplicate page index".into());
                }
                let degrees = params.get("degrees").and_then(Value::as_i64).ok_or("degrees is required")?;
                if ![-270, -180, -90, 90, 180, 270].contains(&degrees) {
                    return Err("rotation must be a quarter turn".into());
                }
                Some(Edit::RotatePages { pages, degrees })
            }
            "content.addText" => {
                let page = index("page")?;
                let text = params.get("text").and_then(Value::as_str).ok_or("text is required")?;
                let rect = params.get("rect").and_then(Value::as_array).ok_or("rect is required")?;
                if page >= page_count || text.is_empty() || text.len() > 65536 || rect.len() != 4 {
                    return Err("invalid page, text or rectangle".into());
                }
                let rect = rect
                    .iter()
                    .map(|n| n.as_f64().filter(|n| n.is_finite() && n.abs() <= 1_000_000.0).ok_or("invalid rectangle".to_string()))
                    .collect::<Result<Vec<_>, _>>()?;
                let [x0, y0, x1, y1] = [rect[0], rect[1], rect[2], rect[3]];
                let size = number("size", 12.0)?;
                if x1 <= x0 || y1 <= y0 || !(1.0..=144.0).contains(&size) {
                    return Err("invalid text bounds or font size".into());
                }
                Some(Edit::AddText { page, text: AddedText { text: text.into(), rect: [x0, y0, x1, y1], size, ..AddedText::default() } })
            }
            "edit.undo" | "edit.redo" => {
                let enabled = if command == "edit.undo" { doc.can_undo().is_some() } else { doc.can_redo().is_some() };
                if !enabled {
                    return Err("no history step is available".into());
                }
                if !self.execute(command) {
                    return Err("history command was disabled".into());
                }
                None
            }
            _ => return Err("unsupported host command".into()),
        };
        if let Some(edit) = edit
            && !self.apply_edit(edit)
        {
            return Err("engine refused the edit".into());
        }
        let after = self.session.get(id).ok_or("document changed unexpectedly")?.edit_generation();
        if after == before {
            return Err("no document mutation was confirmed".into());
        }
        Ok(json!({"documentId":id.0,"engineRevision":after,"dirty":self.host_has_unsaved_documents()}))
    }
}

const HOST_COMMANDS: &[(&str, &str)] = &[
    ("document.setInfo", "Set PDF metadata"),
    ("pages.rotate", "Rotate PDF pages"),
    ("content.addText", "Add text to a PDF page"),
    ("edit.undo", "Undo"),
    ("edit.redo", "Redo"),
];

#[cfg(test)]
mod tests;
