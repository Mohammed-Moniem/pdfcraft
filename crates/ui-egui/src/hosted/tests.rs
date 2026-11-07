use super::*;
use pdfcraft_engine::{AddedContent, Edit};
use serde_json::json;
use std::cell::RefCell;

type Write = (u64, DocId, u64, Vec<u8>);
fn app() -> PdfCraftApp {
    let mut app = PdfCraftApp::new();
    let bytes = app.session.create_from_text("Synthetic host fixture", "PDF host fixture").unwrap();
    app.open_from_host("Fixture.pdf", bytes.as_ref().clone()).unwrap();
    app
}
fn writer(app: &mut PdfCraftApp) -> Rc<RefCell<Vec<Write>>> {
    let writes = Rc::new(RefCell::new(Vec::new()));
    let out = writes.clone();
    let queue = app.host_saves.clone();
    app.host_save_versioned = Some(Box::new(move |_, bytes, _, document, generation| {
        let ticket = queue.start()?;
        out.borrow_mut().push((ticket, document, generation, bytes.to_vec()));
        Ok(ticket)
    }));
    writes
}
fn edit(app: &mut PdfCraftApp, value: &str) {
    app.host_execute("document.setInfo", &json!({"key":"Title","value":value})).unwrap();
}
fn ack(app: &mut PdfCraftApp, ticket: u64, outcome: SaveOutcome) {
    app.host_saves.finish(ticket, outcome);
    app.poll_host_saves();
}

#[test]
fn host_save_clean_document_is_owned_until_matching_acknowledgement() {
    let mut app = app();
    let writes = writer(&mut app);
    let before = app.host_document_identity().unwrap();
    assert!(!app.host_has_unsaved_documents());
    assert!(!app.save_active(SaveTarget::InPlace));
    assert_eq!(app.host_document_identity(), Some(before));
    assert!(app.host_has_unsaved_documents());
    assert_eq!(app.host_pending_saves(), 1);
    assert!(app.unsaved_flag.load(std::sync::atomic::Ordering::Relaxed));
    app.host_saves.finish(999, SaveOutcome::Saved);
    app.poll_host_saves();
    assert_eq!(app.host_pending_saves(), 1);
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Saved);
    assert!(!app.host_has_unsaved_documents());
    assert!(!app.unsaved_flag.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(app.host_pending_saves(), 0);
    assert!(app.host_document_identity().unwrap().1 > before.1);
}

#[test]
fn host_save_failure_keeps_uncertain_owner_and_fresh_retry_can_settle_it() {
    let mut app = app();
    let writes = writer(&mut app);
    app.save_active(SaveTarget::InPlace);
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Failed);
    assert!(app.host_has_unsaved_documents());
    assert!(app.close_from_host(false).is_err());
    app.save_active(SaveTarget::InPlace);
    ack(&mut app, writes.borrow()[1].0, SaveOutcome::Saved);
    assert!(!app.host_has_unsaved_documents());
}

#[test]
fn host_save_stale_success_cannot_rebase_or_clear_newer_edits() {
    let mut app = app();
    let writes = writer(&mut app);
    edit(&mut app, "First");
    app.save_active(SaveTarget::InPlace);
    edit(&mut app, "Newer");
    let current = app.host_document_identity();
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Saved);
    assert_eq!(app.host_document_identity(), current);
    assert!(app.host_has_unsaved_documents());
    assert_eq!(app.host_inspect().unwrap()["metadata"]["Title"], "Newer");
    let bytes = app.host_export().unwrap();
    let mut reopened = PdfCraftApp::new();
    reopened.open_from_host("Roundtrip.pdf", bytes).unwrap();
    assert_eq!(reopened.host_inspect().unwrap()["metadata"]["Title"], "Newer");
    app.save_active(SaveTarget::InPlace);
    ack(&mut app, writes.borrow()[1].0, SaveOutcome::Saved);
    assert!(!app.host_has_unsaved_documents());
}

#[test]
fn host_ack_preserves_uncommitted_ui_drafts_even_without_generation_change() {
    let mut app = app();
    let writes = writer(&mut app);
    app.save_active(SaveTarget::InPlace);
    let before = app.host_document_identity();
    app.views[0].pending_edit = Some(Edit::SetInfo { key: "Title".into(), value: "Human draft".into() });
    assert!(app.host_has_drafts());
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Saved);
    assert_eq!(app.host_document_identity(), before);
    assert!(app.views[0].pending_edit.is_some());
    assert!(app.host_has_unsaved_documents());
    assert!(app.host_export().is_err());
    assert!(app.host_execute("pages.rotate", &json!({"pages":[0],"degrees":90})).is_err());
}

#[test]
fn host_open_and_close_cannot_hide_dirty_documents_parked_by_human_ui() {
    let mut app = app();
    edit(&mut app, "Owned unsaved document");
    let original = app.host_document_identity().unwrap().0;
    let bytes = app.session.create_from_text("Second", "Second synthetic PDF").unwrap();
    assert!(app.open_from_host("Refused.pdf", bytes.as_ref().clone()).is_err());
    assert_eq!(app.views.len(), 1);
    app.open_bytes("Human-opened.pdf", None, bytes.as_ref().clone()).unwrap();
    assert_eq!(app.views.len(), 2);
    assert_ne!(app.host_document_identity().unwrap().0, original);
    assert!(!app.session.get(app.host_document_identity().unwrap().0).unwrap().dirty);
    assert!(app.host_has_unsaved_documents());
    assert!(app.open_from_host("Third.pdf", bytes.as_ref().clone()).is_err());
    assert!(app.close_from_host(true).is_err());
    app.active = Some(0);
    assert_eq!(app.host_inspect().unwrap()["metadata"]["Title"], "Owned unsaved document");
}

#[test]
fn host_pending_ack_blocks_replacement_and_human_discard_of_its_document() {
    let mut app = app();
    let writes = writer(&mut app);
    app.save_active(SaveTarget::InPlace);
    let bytes = app.host_export().unwrap();
    assert!(app.open_from_host("Replacement.pdf", bytes.clone()).is_err());
    assert!(app.close_from_host(true).is_err());
    app.close_tab(0);
    assert_eq!(app.views.len(), 1);
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Saved);
    app.open_from_host("Replacement.pdf", bytes).unwrap();
    assert_eq!(app.views.len(), 2);
}

#[test]
fn host_save_changes_close_continuation_waits_for_current_snapshot_ack() {
    let mut app = app();
    let writes = writer(&mut app);
    edit(&mut app, "Save before closing");
    app.request_close_tab(0);
    app.resolve_close(&egui::Context::default(), Some(true));
    assert_eq!(app.views.len(), 1);
    assert_eq!(app.host_pending_saves(), 1);
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Saved);
    assert!(app.views.is_empty());
    assert!(!app.host_has_unsaved_documents());
}

#[test]
fn host_save_changes_close_continuation_cannot_discard_edits_made_during_write() {
    let mut app = app();
    let writes = writer(&mut app);
    edit(&mut app, "First");
    app.request_close_tab(0);
    app.resolve_close(&egui::Context::default(), Some(true));
    edit(&mut app, "Keep this later edit");
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Saved);
    assert_eq!(app.views.len(), 1);
    assert!(app.host_has_unsaved_documents());
    assert_eq!(app.host_inspect().unwrap()["metadata"]["Title"], "Keep this later edit");
}

#[test]
fn host_commands_are_real_bounded_and_roundtrip_added_text_without_marking_saved() {
    let mut app = app();
    let before = app.host_document_identity().unwrap();
    if let Ok(directory) = std::env::var("PDFCRAFT_HOST_FIXTURE_DIR") {
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(std::path::Path::new(&directory).join("synthetic-input.pdf"), app.host_export().unwrap()).unwrap();
    }
    let commands = app.host_commands("");
    assert_eq!(commands.as_array().unwrap().iter().find(|command| command["id"] == "edit.undo").unwrap()["enabled"], false);
    assert!(app.host_execute("file.open", &json!({})).is_err());
    assert!(app.host_execute("pages.rotate", &json!({"pages":[999],"degrees":90})).is_err());
    assert!(app.host_execute("document.setInfo", &json!({"key":"UnsafeKey","value":"x"})).is_err());
    assert_eq!(app.host_document_identity(), Some(before));
    app.host_execute("content.addText", &json!({"page":0,"text":"PDF_BRIDGE_MARKER","rect":[72,100,400,140],"size":14})).unwrap();
    assert_eq!(app.host_commands("undo")[0]["enabled"], true);
    app.host_execute("pages.rotate", &json!({"pages":[0],"degrees":90})).unwrap();
    let edited = app.host_document_identity().unwrap();
    app.host_execute("edit.undo", &json!({})).unwrap();
    app.host_execute("edit.redo", &json!({})).unwrap();
    assert!(app.host_document_identity().unwrap().1 > edited.1);
    let bytes = app.host_export().unwrap();
    if let Ok(directory) = std::env::var("PDFCRAFT_HOST_FIXTURE_DIR") {
        std::fs::write(std::path::Path::new(&directory).join("synthetic-roundtrip.pdf"), &bytes).unwrap();
    }
    assert!(app.host_has_unsaved_documents());
    let mut reopened = PdfCraftApp::new();
    reopened.open_from_host("Roundtrip.pdf", bytes).unwrap();
    let id = reopened.host_document_identity().unwrap().0;
    assert!(
        reopened
            .session
            .get(id)
            .unwrap()
            .added()
            .iter()
            .any(|item| matches!(&item.content,AddedContent::Text(text) if text.text=="PDF_BRIDGE_MARKER"))
    );
    assert_eq!(reopened.host_inspect().unwrap()["pages"][0]["rotation"], 90);
}

#[test]
fn host_save_tickets_are_bounded_single_terminal_and_late_results_are_ignored() {
    let queue = SaveQueue::default();
    let ticket = queue.start().unwrap();
    queue.finish(ticket, SaveOutcome::Failed);
    queue.finish(ticket, SaveOutcome::Saved);
    queue.finish(999, SaveOutcome::Saved);
    assert_eq!(queue.take_finished(), vec![(ticket, SaveOutcome::Failed)]);
    assert!(queue.take_finished().is_empty());
    for _ in 0..8 {
        queue.start().unwrap();
    }
    assert!(queue.start().is_err());
}

#[test]
fn host_overlapping_identical_snapshots_stay_owned_until_the_last_ack() {
    let mut app = app();
    let writes = writer(&mut app);
    edit(&mut app, "Two saves");
    app.save_active(SaveTarget::InPlace);
    app.save_active(SaveTarget::InPlace);
    let before = app.host_document_identity();
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Saved);
    assert!(app.host_has_unsaved_documents());
    assert_eq!(app.host_document_identity(), before);
    ack(&mut app, writes.borrow()[1].0, SaveOutcome::Saved);
    assert!(!app.host_has_unsaved_documents());
}

fn save_changes_close(app: &mut PdfCraftApp, index: usize) {
    app.request_close_tab(index);
    assert!(app.close_request.is_some());
    close_prompt_key(app, egui::Key::Enter);
}

fn close_prompt_key(app: &mut PdfCraftApp, key: egui::Key) {
    let ctx = egui::Context::default();
    let input = egui::RawInput {
        events: vec![egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }],
        ..Default::default()
    };
    let mut answered = false;
    let mut output = ctx.run_ui(input, |_| {
        if let Some(choice) = crate::dialogs::save_prompt_key(&ctx, &mut app.save_prompt_button) {
            app.resolve_close(&ctx, choice);
            answered = true;
        }
    });
    // This input-only lifecycle test has no GPU renderer. Explicitly consume its font delta.
    output.textures_delta.clear();
    assert!(answered, "the actual close prompt must consume the human key");
}

#[test]
fn host_overlapping_save_changes_close_survives_both_ack_orders_and_save_as() {
    for reverse in [false, true] {
        for target in [SaveTarget::InPlace, SaveTarget::As] {
            let mut app = app();
            let writes = writer(&mut app);
            edit(&mut app, "Two persisted copies, one requested close");
            save_changes_close(&mut app, 0);
            app.save_active(target);
            let writes = writes.borrow().clone();
            assert_eq!(writes.len(), 2);
            assert_eq!(writes[0].3, writes[1].3);
            let order = if reverse { [1, 0] } else { [0, 1] };
            ack(&mut app, writes[order[0]].0, SaveOutcome::Saved);
            assert_eq!(app.views.len(), 1, "the first ACK cannot close through a pending write");
            assert!(app.host_has_unsaved_documents());
            assert_eq!(app.host_pending_saves(), 2, "retained owner group includes its acknowledged continuation");
            ack(&mut app, writes[order[1]].0, SaveOutcome::Saved);
            assert!(app.views.is_empty(), "the requested Close must survive either ACK order");
            assert!(!app.host_has_unsaved_documents());
            assert_eq!(app.host_pending_saves(), 0);
        }
    }
}

#[test]
fn host_overlapping_failed_or_cancelled_write_vetoes_deferred_close_in_either_order() {
    for failed in [0, 1] {
        for reverse in [false, true] {
            let mut app = app();
            let writes = writer(&mut app);
            edit(&mut app, "Retain uncertain work");
            save_changes_close(&mut app, 0);
            app.save_active(SaveTarget::InPlace);
            let writes = writes.borrow().clone();
            let order = if reverse { [1, 0] } else { [0, 1] };
            for index in order {
                ack(&mut app, writes[index].0, if index == failed { SaveOutcome::Failed } else { SaveOutcome::Saved });
            }
            assert_eq!(app.views.len(), 1);
            assert_eq!(
                app.host_has_unsaved_documents(),
                failed == 1,
                "the latest current confirmed snapshot may settle; any failure still vetoes Close"
            );
            assert_eq!(app.host_pending_saves(), 0);
            // A cancelled ticket is terminal: a late success cannot revive its Close action.
            ack(&mut app, writes[failed].0, SaveOutcome::Saved);
            assert_eq!(app.views.len(), 1);
            assert_eq!(app.host_has_unsaved_documents(), failed == 1);
            // An explicit fresh save can settle the snapshot, without reviving a failed Close.
            let retry = writer(&mut app);
            app.save_active(SaveTarget::InPlace);
            ack(&mut app, retry.borrow()[0].0, SaveOutcome::Saved);
            assert_eq!(app.views.len(), 1);
            assert!(!app.host_has_unsaved_documents());
        }
    }
}

#[test]
fn host_immediate_successful_retry_can_clean_before_failed_ticket_was_polled_without_reviving_close() {
    for closing in [false, true] {
        let mut app = app();
        let writes = writer(&mut app);
        edit(&mut app, "Retry this actual snapshot");
        if closing {
            save_changes_close(&mut app, 0);
        } else {
            app.save_active(SaveTarget::InPlace);
        }
        // The browser may reject the first request and immediately dispatch a new request
        // before the next UI frame consumes the failed ticket.
        app.host_saves.finish(writes.borrow()[0].0, SaveOutcome::Failed);
        app.save_active(SaveTarget::InPlace);
        assert_eq!(app.host_pending_saves(), 2);
        app.host_saves.finish(writes.borrow()[1].0, SaveOutcome::Saved);
        app.poll_host_saves();
        assert_eq!(app.views.len(), 1, "a failed Close action cannot be revived by a retry");
        assert_eq!(app.host_pending_saves(), 0);
        assert!(!app.host_has_unsaved_documents(), "the successful latest snapshot must become clean");
    }
}

#[test]
fn host_synchronous_overlap_failure_vetoes_earlier_close() {
    let mut app = app();
    let writes = writer(&mut app);
    edit(&mut app, "Failed overlapping callback");
    save_changes_close(&mut app, 0);
    app.host_save_versioned = Some(Box::new(|_, _, _, _, _| Err("storage unavailable".into())));
    app.save_active(SaveTarget::InPlace);
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Saved);
    assert_eq!(app.views.len(), 1);
    assert!(app.host_has_unsaved_documents());
}

#[test]
fn host_newer_edit_between_overlap_acks_vetoes_close_even_when_latest_write_succeeds() {
    for reverse in [false, true] {
        let mut app = app();
        let writes = writer(&mut app);
        edit(&mut app, "Save snapshot");
        save_changes_close(&mut app, 0);
        app.save_active(SaveTarget::InPlace);
        let writes = writes.borrow().clone();
        let order = if reverse { [1, 0] } else { [0, 1] };
        ack(&mut app, writes[order[0]].0, SaveOutcome::Saved);
        edit(&mut app, "Keep later edit");
        ack(&mut app, writes[order[1]].0, SaveOutcome::Saved);
        assert_eq!(app.views.len(), 1);
        assert!(app.host_has_unsaved_documents());
        assert_eq!(app.host_inspect().unwrap()["metadata"]["Title"], "Keep later edit");
    }
}

#[test]
fn host_later_human_close_cancel_withdraws_retained_close_but_not_successful_storage() {
    for reverse in [false, true] {
        let mut app = app();
        let writes = writer(&mut app);
        edit(&mut app, "Persist but keep open after Cancel");
        save_changes_close(&mut app, 0);
        app.save_active(SaveTarget::InPlace);
        let writes = writes.borrow().clone();
        let order = if reverse { [1, 0] } else { [0, 1] };
        ack(&mut app, writes[order[0]].0, SaveOutcome::Saved);
        app.request_close_tab(0);
        close_prompt_key(&mut app, egui::Key::Escape);
        ack(&mut app, writes[order[1]].0, SaveOutcome::Saved);
        assert_eq!(app.views.len(), 1);
        assert!(!app.host_has_unsaved_documents());
    }
}

#[test]
fn host_overlap_export_copy_does_not_release_owner_or_drop_parked_close_continuation() {
    for reverse in [false, true] {
        let mut app = app();
        let writes = writer(&mut app);
        edit(&mut app, "Parked document");
        let owner = app.host_document_identity().unwrap().0;
        save_changes_close(&mut app, 0);
        app.save_active(SaveTarget::As);
        let exported = app.host_export().unwrap();
        assert_eq!(exported, writes.borrow()[0].3);
        assert_eq!(app.host_pending_saves(), 2);
        assert!(app.host_has_unsaved_documents());
        let second = app.session.create_from_text("Human second", "Second synthetic PDF").unwrap();
        app.open_bytes("Second.pdf", None, second.as_ref().clone()).unwrap();
        let active = app.host_document_identity().unwrap().0;
        assert_ne!(active, owner);
        let writes = writes.borrow().clone();
        let order = if reverse { [1, 0] } else { [0, 1] };
        ack(&mut app, writes[order[0]].0, SaveOutcome::Saved);
        assert_eq!(app.views.len(), 2);
        assert_eq!(app.host_document_identity().unwrap().0, active);
        ack(&mut app, writes[order[1]].0, SaveOutcome::Saved);
        assert_eq!(app.views.len(), 1);
        assert!(app.session.get(owner).is_none());
        assert_eq!(app.host_document_identity().unwrap().0, active);
        assert!(!app.host_has_unsaved_documents());
    }
}

#[test]
fn host_ack_group_retention_is_bounded_and_drafts_veto_late_close() {
    let mut app = app();
    let writes = writer(&mut app);
    edit(&mut app, "Bounded close group");
    save_changes_close(&mut app, 0);
    for _ in 1..8 {
        app.save_active(SaveTarget::InPlace);
    }
    let writes = writes.borrow().clone();
    assert_eq!(writes.len(), 8);
    for write in writes.iter().take(7) {
        ack(&mut app, write.0, SaveOutcome::Saved);
    }
    app.save_active(SaveTarget::InPlace);
    assert_eq!(app.host_pending_saves(), 8);
    // This pending human field belongs to the owner even if its generation did not advance.
    app.views[0].pending_edit = Some(Edit::SetInfo { key: "Title".into(), value: "Keep human draft".into() });
    ack(&mut app, writes[7].0, SaveOutcome::Saved);
    assert_eq!(app.host_pending_saves(), 0);
    assert_eq!(app.views.len(), 1);
    assert!(app.views[0].pending_edit.is_some());
    assert!(app.host_has_unsaved_documents());
}

#[test]
fn host_new_human_prompt_is_not_closed_underneath_by_an_earlier_save() {
    let mut app = app();
    let writes = writer(&mut app);
    edit(&mut app, "Preserve the newer question");
    save_changes_close(&mut app, 0);
    app.request_close_tab(0);
    let newer_question = app.close_request;
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Saved);
    assert_eq!(app.views.len(), 1);
    assert_eq!(app.close_request, newer_question);
    assert!(!app.host_has_unsaved_documents());
    close_prompt_key(&mut app, egui::Key::Escape);
    assert_eq!(app.views.len(), 1);
}

#[test]
fn host_operations_cannot_act_under_a_pending_human_modal() {
    let mut app = app();
    let before = app.host_document_identity();
    let id = before.unwrap().0;
    app.close_request = Some(crate::CloseRequest::Tab(id));
    assert!(app.host_interaction_busy());
    assert!(app.host_export().is_err());
    assert!(app.host_execute("document.setInfo", &json!({"key":"Title","value":"Hidden edit"})).is_err());
    assert!(app.close_from_host(true).is_err());
    assert_eq!(app.host_document_identity(), before);
    assert_eq!(app.host_commands("undo")[0]["enabled"], false);
}

#[test]
fn host_wire_snapshot_revision_is_stable_while_only_storage_ownership_changes() {
    let mut app = app();
    let writes = writer(&mut app);
    let mut clock = RevisionClock::default();
    let (id, generation) = app.host_document_identity().unwrap();
    let snapshot = clock.sync(id.0, generation);
    app.save_active(SaveTarget::InPlace);
    assert!(app.host_has_unsaved_documents());
    let (id, generation) = app.host_document_identity().unwrap();
    assert_eq!(clock.sync(id.0, generation), snapshot, "pending storage must not stale the exact bytes just sent");
    ack(&mut app, writes.borrow()[0].0, SaveOutcome::Saved);
    assert!(!app.host_has_unsaved_documents());
    let (id, generation) = app.host_document_identity().unwrap();
    assert!(clock.sync(id.0, generation) > snapshot, "adopting saved bytes changes the real engine generation");
    let before = clock.sync(id.0, generation);
    edit(&mut app, "Human-visible edit");
    let (id, generation) = app.host_document_identity().unwrap();
    assert!(clock.sync(id.0, generation) > before);
    let before_switch = clock.sync(id.0, generation);
    assert!(clock.sync(id.0 + 1, 0) > before_switch, "switching engine identity must not reset the wire revision");
}
