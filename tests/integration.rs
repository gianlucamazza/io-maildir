//! End-to-end Maildir test flow.
//!
//! Drives the full [`MaildirClient`] surface against a freshly created
//! tempdir. The test is autonomous: it provisions its own Maildirs and
//! its own messages, then exercises every public operation in
//! sequence:
//!
//! ```text
//! NEW CLIENT
//!   → MAILDIR LIST              (baseline: empty)
//!   → MAILDIR CREATE inbox / drafts
//!   → MAILDIR LIST              (verify both visible)
//!   → MESSAGE STORE x3          (/new, /cur, /cur with Seen)
//!   → LIST ENTRIES              (verify count + ids, no body read)
//!   → READ ENTRY                (single, body round-trip)
//!   → READ ENTRIES              (sequential bulk)
//!   → READ ENTRIES PAR          (parallel bulk, same result as seq)
//!   → GET                       (full message by id)
//!   → LOCATE                    (path + subdir + flags by id)
//!   → FLAGS SET on /new         (no-op: verify path/subdir/flags unchanged)
//!   → FLAGS SET Seen on /cur    (renames file in place)
//!   → FLAGS ADD Flagged         (verify Seen + Flagged both present)
//!   → FLAGS REMOVE Seen         (verify only Flagged remains)
//!   → COPY inbox→drafts         (fresh id minted, body + flags preserved)
//!   → MOVE inbox→drafts         (entry_c, fresh id, inbox shrinks)
//!   → RENAME drafts → archive   (verify old gone, new exists)
//!   → DELETE MAILDIR archive    (verify dir gone)
//!   → MAILDIR LIST              (final state: only inbox)
//! ```

use std::path::Path;

use io_maildir::{
    client::MaildirClient,
    flag::types::{MaildirFlag, MaildirFlags},
    maildir::types::MaildirSubdir,
    path::FsPath,
};
use tempfile::tempdir;

#[test]
fn end_to_end() {
    let _ = env_logger::try_init();

    let dir = tempdir().expect("create tempdir");
    let root = FsPath::new(dir.path().to_string_lossy().into_owned());
    let client = MaildirClient::new(root.clone());

    // ── MAILDIR LIST (baseline) ─────────────────────────────────────

    let maildirs = client.list_maildirs().expect("list maildirs (baseline)");
    assert!(maildirs.is_empty(), "root should be empty initially");

    // ── MAILDIR CREATE ──────────────────────────────────────────────

    client.create_maildir("inbox").expect("create inbox");
    client.create_maildir("drafts").expect("create drafts");

    let inbox = client.load_maildir("inbox").expect("load inbox");
    let drafts = client.load_maildir("drafts").expect("load drafts");

    for maildir in [&inbox, &drafts] {
        assert!(Path::new(maildir.cur().as_str()).is_dir());
        assert!(Path::new(maildir.new().as_str()).is_dir());
        assert!(Path::new(maildir.tmp().as_str()).is_dir());
    }

    // ── MAILDIR LIST (after create) ─────────────────────────────────

    let maildirs = client.list_maildirs().expect("list maildirs");
    assert_eq!(maildirs.len(), 2);
    assert!(maildirs.contains(&inbox));
    assert!(maildirs.contains(&drafts));

    // ── MESSAGE STORE x3 ────────────────────────────────────────────

    let body_a = build_eml("alice@example.org", "first");
    let body_b = build_eml("bob@example.org", "second");
    let body_c = build_eml("carol@example.org", "third");

    let (id_a, path_a) = client
        .store(
            inbox.clone(),
            MaildirSubdir::New,
            MaildirFlags::default(),
            body_a.clone().into_bytes(),
        )
        .expect("store first message");
    let (id_b, path_b) = client
        .store(
            inbox.clone(),
            MaildirSubdir::Cur,
            MaildirFlags::default(),
            body_b.clone().into_bytes(),
        )
        .expect("store second message");
    let (id_c, path_c) = client
        .store(
            inbox.clone(),
            MaildirSubdir::Cur,
            MaildirFlags::from_iter([MaildirFlag::Seen]),
            body_c.clone().into_bytes(),
        )
        .expect("store third message");

    for path in [&path_a, &path_b, &path_c] {
        assert!(Path::new(path.as_str()).is_file());
    }
    assert!(path_a.starts_with(&inbox.new()), "id_a should live in /new");
    assert!(path_b.starts_with(&inbox.cur()), "id_b should live in /cur");
    assert!(
        path_c.starts_with(&inbox.cur()),
        "id_c should live in /cur with flags",
    );
    assert_ne!(id_a, id_b);
    assert_ne!(id_b, id_c);
    assert_ne!(id_a, id_c);

    // ── LIST ENTRIES (no body read) ─────────────────────────────────

    let entries = client.list_entries(inbox.clone()).expect("list entries");
    assert_eq!(entries.len(), 3, "expected three entries after store");

    let listed_ids: Vec<&str> = entries.iter().filter_map(|e| e.id()).collect();
    for id in [&id_a, &id_b, &id_c] {
        assert!(
            listed_ids.contains(&id.as_str()),
            "id {id} missing from listing",
        );
    }

    // Flags decoded from filename round-trip via MaildirEntry::flags
    let entry_c = entries
        .iter()
        .find(|e| e.id() == Some(id_c.as_str()))
        .expect("locate entry_c");
    assert!(
        entry_c.flags().contains(&MaildirFlag::Seen),
        "entry_c should carry the Seen flag in its filename",
    );

    // ── READ ENTRY (single round-trip) ──────────────────────────────

    let entry_a = entries
        .iter()
        .find(|e| e.id() == Some(id_a.as_str()))
        .expect("locate entry_a");
    let msg_a = client.read_entry(entry_a).expect("read entry_a");
    assert_eq!(msg_a.id(), Some(id_a.as_str()));
    assert_eq!(msg_a.contents(), body_a.as_bytes());

    // ── READ ENTRIES (sequential bulk) ──────────────────────────────

    let entries_vec: Vec<_> = entries.iter().cloned().collect();
    let bulk_seq = client
        .read_entries(&entries_vec)
        .expect("read entries (sequential)");
    assert_eq!(bulk_seq.len(), 3);
    let seq_ids: Vec<&str> = bulk_seq.iter().filter_map(|m| m.id()).collect();
    for id in [&id_a, &id_b, &id_c] {
        assert!(seq_ids.contains(&id.as_str()));
    }

    // ── READ ENTRIES PAR (parallel bulk, identical result) ──────────

    let bulk_par = client
        .read_entries_par(&entries_vec)
        .expect("read entries (parallel)");
    assert_eq!(
        bulk_par, bulk_seq,
        "sequential and parallel bulk reads must produce the same set",
    );

    // ── GET (full message by id) ────────────────────────────────────

    let fetched = client.get(inbox.clone(), &id_b).expect("get entry_b");
    assert_eq!(fetched.id(), Some(id_b.as_str()));
    assert_eq!(fetched.contents(), body_b.as_bytes());

    // ── LOCATE (path + subdir + flags) ──────────────────────────────

    let (located_path, located_subdir, located_flags) =
        client.locate(inbox.clone(), &id_a).expect("locate entry_a");
    assert_eq!(located_path, path_a);
    assert_eq!(located_subdir, MaildirSubdir::New);
    assert!(located_flags.is_empty(), "entry_a stored without flags");

    // ── FLAGS SET (no-op for /new) ──────────────────────────────────

    // Per the Maildir spec, only /cur carries flags: ops on /new and
    // /tmp messages are documented no-ops.
    client
        .set_flags(
            inbox.clone(),
            &id_a,
            MaildirFlags::from_iter([MaildirFlag::Seen]),
        )
        .expect("set flags on /new entry_a (no-op)");
    let (after_noop_path, after_noop_subdir, after_noop_flags) = client
        .locate(inbox.clone(), &id_a)
        .expect("locate entry_a after no-op set");
    assert_eq!(after_noop_subdir, MaildirSubdir::New);
    assert_eq!(after_noop_path, path_a);
    assert!(after_noop_flags.is_empty());

    // ── FLAGS SET (renames /cur file) ───────────────────────────────

    client
        .set_flags(
            inbox.clone(),
            &id_b,
            MaildirFlags::from_iter([MaildirFlag::Seen]),
        )
        .expect("set flags Seen on entry_b");

    let (after_set_path, after_set_subdir, after_set_flags) = client
        .locate(inbox.clone(), &id_b)
        .expect("locate entry_b after set");
    assert_eq!(after_set_subdir, MaildirSubdir::Cur);
    assert!(after_set_flags.contains(&MaildirFlag::Seen));
    assert!(
        !Path::new(path_b.as_str()).exists(),
        "old flag-less /cur path should no longer exist",
    );
    assert!(Path::new(after_set_path.as_str()).is_file());

    // ── FLAGS ADD (union with existing) ─────────────────────────────

    client
        .add_flags(
            inbox.clone(),
            &id_b,
            MaildirFlags::from_iter([MaildirFlag::Flagged]),
        )
        .expect("add Flagged on entry_b");

    let (_, _, after_add_flags) = client
        .locate(inbox.clone(), &id_b)
        .expect("locate entry_b after add");
    assert!(after_add_flags.contains(&MaildirFlag::Seen));
    assert!(after_add_flags.contains(&MaildirFlag::Flagged));

    // ── FLAGS REMOVE (subtraction) ──────────────────────────────────

    client
        .remove_flags(
            inbox.clone(),
            &id_b,
            MaildirFlags::from_iter([MaildirFlag::Seen]),
        )
        .expect("remove Seen on entry_b");

    let (_, _, after_remove_flags) = client
        .locate(inbox.clone(), &id_b)
        .expect("locate entry_b after remove");
    assert!(!after_remove_flags.contains(&MaildirFlag::Seen));
    assert!(after_remove_flags.contains(&MaildirFlag::Flagged));

    // ── COPY (inbox → drafts) ───────────────────────────────────────

    // A copy is a fresh delivery into the target: it mints a brand-new
    // unique name (never reuses the source basename) and preserves flags.
    client
        .copy(
            &id_b,
            inbox.clone(),
            drafts.clone(),
            Some(MaildirSubdir::Cur),
        )
        .expect("copy entry_b to drafts");

    assert_eq!(
        client.list_entries(inbox.clone()).unwrap().len(),
        3,
        "inbox still holds 3 entries after copy",
    );
    let drafts_after_copy = client.list_entries(drafts.clone()).unwrap();
    assert_eq!(
        drafts_after_copy.len(),
        1,
        "drafts now holds 1 entry from the copy",
    );
    let copy_b = drafts_after_copy.iter().next().expect("one drafts entry");
    assert_ne!(
        copy_b.id(),
        Some(id_b.as_str()),
        "copy must mint a fresh id, not reuse the source basename",
    );
    assert_eq!(
        client
            .read_entry(copy_b)
            .expect("read copied entry")
            .contents(),
        body_b.as_bytes(),
        "copy must carry the source body",
    );
    // entry_b carries only Flagged at this point (Seen was removed above).
    assert!(
        copy_b.flags().contains(&MaildirFlag::Flagged),
        "copy must preserve the source flags",
    );

    // ── MOVE (inbox → drafts, entry_c) ──────────────────────────────

    // A move relocates the entry under a fresh unique name as well.
    client
        .r#move(
            &id_c,
            inbox.clone(),
            drafts.clone(),
            Some(MaildirSubdir::Cur),
        )
        .expect("move entry_c to drafts");

    assert_eq!(
        client.list_entries(inbox.clone()).unwrap().len(),
        2,
        "inbox should hold 2 entries after move",
    );
    let drafts_after_move = client.list_entries(drafts.clone()).unwrap();
    assert_eq!(
        drafts_after_move.len(),
        2,
        "drafts should hold 2 entries after move",
    );
    assert!(
        drafts_after_move
            .iter()
            .all(|e| e.id() != Some(id_c.as_str())),
        "move must mint a fresh id, not reuse the source basename",
    );

    // ── RENAME (drafts → archive) ───────────────────────────────────

    client
        .rename_maildir("drafts", "archive")
        .expect("rename drafts → archive");
    assert!(
        !Path::new(root.join("drafts").as_str()).exists(),
        "old drafts path should be gone",
    );
    assert!(
        Path::new(root.join("archive").as_str()).is_dir(),
        "new archive path should exist",
    );

    let archive = client
        .load_maildir("archive")
        .expect("load archive after rename");

    // The copy of body_b lives in archive under a fresh id; find it by
    // content (not by id_b, which the copy no longer carries) and delete it.
    let archive_entries = client
        .list_entries(archive.clone())
        .expect("list archive entries");
    let archived_b = archive_entries
        .iter()
        .find(|e| {
            client
                .read_entry(e)
                .map(|m| m.contents() == body_b.as_bytes())
                .unwrap_or(false)
        })
        .expect("locate copy of body_b in archive");
    std::fs::remove_file(archived_b.path().as_str()).expect("remove archived copy of body_b");
    assert_eq!(
        client.list_entries(archive.clone()).unwrap().len(),
        1,
        "archive should hold 1 entry after manual file removal",
    );

    // ── DELETE MAILDIR ──────────────────────────────────────────────

    client.delete_maildir("archive").expect("delete archive");
    assert!(
        !Path::new(archive.path().as_str()).exists(),
        "archive dir should be removed",
    );

    let maildirs = client.list_maildirs().expect("list after delete");
    assert_eq!(maildirs.len(), 1, "only inbox should remain");
    assert!(maildirs.contains(&inbox));
}

fn build_eml(from: &str, tag: &str) -> String {
    [
        &format!("From: io-maildir test <{from}>"),
        &format!("To: io-maildir test <{from}>"),
        &format!("Subject: io-maildir integration test {tag}"),
        "Date: Thu, 01 Jan 2026 00:00:00 +0000",
        "MIME-Version: 1.0",
        "Content-Type: text/plain; charset=utf-8",
        "",
        &format!("This is automated test email {tag} from io-maildir tests."),
    ]
    .join("\r\n")
}
