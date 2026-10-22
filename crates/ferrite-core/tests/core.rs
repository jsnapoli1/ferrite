use chrono::{Duration, Utc};
use ferrite_core::auth::NewUser;
use ferrite_core::collab::DocKind;
use ferrite_core::files::{WriteOpts, perm};
use ferrite_core::shares::{NewShare, ShareUpdate};
use ferrite_core::talk::{Actor, NewMessage};
use ferrite_core::{Config, Core, Error};
use serde_json::Value;
use yrs::{GetString, Text, Transact};

async fn next(rx: &mut ferrite_core::events::EventRx) -> Value {
    let raw = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("event within 5s")
        .expect("channel open");
    serde_json::from_str(&raw).unwrap()
}

async fn core() -> (Core, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_dir: dir.path().to_path_buf(),
        max_versions: 3,
        ..Config::default()
    };
    (Core::open(cfg).await.unwrap(), dir)
}

async fn user(core: &Core, name: &str) -> String {
    core.create_user(NewUser {
        username: name.into(),
        display_name: Some(name.to_uppercase()),
        password: "password123".into(),
        ..Default::default()
    })
    .await
    .unwrap()
    .id
}

#[tokio::test]
async fn auth_tokens_and_groups() {
    let (core, _d) = core().await;
    let alice = user(&core, "alice").await;
    assert!(matches!(
        core.create_user(NewUser {
            username: "ALICE".into(),
            password: "password123".into(),
            ..Default::default()
        })
        .await,
        Err(Error::Conflict(_))
    ));
    assert!(core.authenticate("alice", "password123").await.is_ok());
    assert!(matches!(core.authenticate("alice", "nope").await, Err(Error::Unauthorized)));
    assert!(matches!(core.authenticate("ghost", "password123").await, Err(Error::Unauthorized)));

    let (info, secret) = core.create_token(&alice, "app", "laptop", "files,mcp", None).await.unwrap();
    let (u, t) = core.resolve_token(&secret).await.unwrap();
    assert_eq!(u.username, "alice");
    assert!(t.allows("mcp") && !t.allows("admin"));
    core.revoke_token(&alice, &info.id).await.unwrap();
    assert!(core.resolve_token(&secret).await.is_err());

    let (_, expired) = core.create_token(&alice, "session", "old", "*", Some(Duration::seconds(-1))).await.unwrap();
    assert!(core.resolve_token(&expired).await.is_err());

    let g = core.create_group("design").await.unwrap();
    core.add_to_group(&g.id, &alice).await.unwrap();
    assert_eq!(core.group_members(&g.id).await.unwrap().len(), 1);
    assert_eq!(core.user_groups(&alice).await.unwrap()[0].name, "design");

    // Disabling a user revokes access.
    let (_, s2) = core.create_token(&alice, "app", "x", "*", None).await.unwrap();
    core.update_user(
        &alice,
        ferrite_core::auth::UserUpdate {
            disabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(core.resolve_token(&s2).await.is_err());
}

#[tokio::test]
async fn files_versions_trash_copy_move() {
    let (core, _d) = core().await;
    let a = user(&core, "alice").await;
    let root = core.ensure_root(&a).await.unwrap();
    let docs = core.mkdir(&a, &root, "Docs").await.unwrap();
    assert!(matches!(core.mkdir(&a, &root, "Docs").await, Err(Error::Conflict(_))));

    let f = core.write_bytes(&a, &docs.id, "plan.md", b"v1", WriteOpts::user()).await.unwrap();
    assert_eq!(f.mime, "text/markdown");
    let f2 = core.write_bytes(&a, &docs.id, "plan.md", b"v2", WriteOpts::user()).await.unwrap();
    assert_eq!(f.id, f2.id);
    assert_ne!(f.etag, f2.etag);
    // Optimistic concurrency.
    let stale = WriteOpts {
        if_match: Some(f.etag.clone()),
        ..WriteOpts::user()
    };
    assert!(matches!(
        core.write_bytes(&a, &docs.id, "plan.md", b"v3", stale).await,
        Err(Error::PreconditionFailed)
    ));
    for i in 3..=6 {
        core.write_bytes(&a, &docs.id, "plan.md", format!("v{i}").as_bytes(), WriteOpts::user())
            .await
            .unwrap();
    }
    let versions = core.list_versions(&a, &f.id).await.unwrap();
    assert_eq!(versions.len(), 3, "pruned to max_versions");
    assert_eq!(core.read_version(&a, &f.id, &versions[0].id).await.unwrap(), b"v5");
    core.restore_version(&a, &f.id, &versions[0].id).await.unwrap();
    assert_eq!(core.read_file(&a, &f.id).await.unwrap().1, b"v5");

    // Path resolution, copy and move.
    assert_eq!(core.resolve_path(&a, "/Docs/plan.md").await.unwrap().id, f.id);
    let copy = core.copy_node(&a, &docs.id, &root, "Docs copy").await.unwrap();
    let copied_file = core.resolve_path(&a, "Docs copy/plan.md").await.unwrap();
    assert_eq!(core.read_file(&a, &copied_file.id).await.unwrap().1, b"v5");
    assert!(core.copy_node(&a, &docs.id, &docs.id, "loop").await.is_err());
    let moved = core.move_node(&a, &copy.id, &docs.id, "Nested", false).await.unwrap();
    assert_eq!(core.display_path(&a, &moved).await.unwrap(), "/Docs/Nested");
    assert!(core.move_node(&a, &docs.id, &moved.id, "x", false).await.is_err(), "no moving into own child");

    // Trash and restore keep the subtree.
    core.delete(&a, &docs.id).await.unwrap();
    assert!(core.resolve_path(&a, "/Docs/plan.md").await.is_err());
    assert_eq!(core.list_trash(&a).await.unwrap().len(), 1);
    core.restore(&a, &docs.id).await.unwrap();
    assert_eq!(
        core.read_file(&a, &core.resolve_path(&a, "/Docs/Nested/plan.md").await.unwrap().id)
            .await
            .unwrap()
            .1,
        b"v5"
    );

    // Purging releases blobs that nothing else references.
    core.delete(&a, &docs.id).await.unwrap();
    core.empty_trash(&a).await.unwrap();
    let blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs").fetch_one(&core.db).await.unwrap();
    assert_eq!(blobs, 0);

    // mkdir_p + write_path
    let n = core.write_path(&a, "/a/b/c.txt", b"deep", WriteOpts::user()).await.unwrap();
    assert_eq!(core.display_path(&a, &n).await.unwrap(), "/a/b/c.txt");
}

#[tokio::test]
async fn dedup_quota_and_locks() {
    let (core, _d) = core().await;
    let a = user(&core, "alice").await;
    let b = user(&core, "bob").await;
    let root = core.ensure_root(&a).await.unwrap();
    core.write_bytes(&a, &root, "x.bin", &[7u8; 1000], WriteOpts::user()).await.unwrap();
    core.write_bytes(&a, &root, "y.bin", &[7u8; 1000], WriteOpts::user()).await.unwrap();
    let blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs").fetch_one(&core.db).await.unwrap();
    assert_eq!(blobs, 1, "identical content stored once");
    assert_eq!(core.quota(&a).await.unwrap().used, 2000);

    core.update_user(
        &a,
        ferrite_core::auth::UserUpdate {
            quota_bytes: Some(Some(2500)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        core.write_bytes(&a, &root, "z.bin", &[1u8; 600], WriteOpts::user()).await,
        Err(Error::QuotaExceeded)
    ));

    // Locks block other writers but not the holder.
    let f = core.write_bytes(&a, &root, "doc.docx", b"PK..", WriteOpts::user()).await.unwrap();
    let folder = core.mkdir(&a, &root, "Team").await.unwrap();
    core.create_share(
        &a,
        &folder.id,
        NewShare {
            kind: "user".into(),
            target: Some("bob".into()),
            permissions: None,
            password: None,
            expires_at: None,
            label: None,
        },
    )
    .await
    .unwrap();
    let lf = core.move_node(&a, &f.id, &folder.id, "doc.docx", false).await.unwrap();
    let lock = core.lock(&a, &lf.id, "wordcraft", 600).await.unwrap();
    assert!(matches!(
        core.write_bytes(&b, &folder.id, "doc.docx", b"bob", WriteOpts::user()).await,
        Err(Error::Locked(_))
    ));
    let with_token = WriteOpts {
        lock_token: Some(lock.token.clone()),
        ..WriteOpts::user()
    };
    core.write_bytes(&a, &folder.id, "doc.docx", b"alice", with_token).await.unwrap();
    assert!(core.unlock(&b, &lf.id).await.is_err());
    core.unlock(&a, &lf.id).await.unwrap();
    core.write_bytes(&b, &folder.id, "doc.docx", b"bob", WriteOpts::user()).await.unwrap();
    let entry = core.list_dir(&a, &folder.id).await.unwrap();
    assert_eq!(entry[0].craft_app, Some("wordcraft"));
}

#[tokio::test]
async fn sharing_user_group_link() {
    let (core, _d) = core().await;
    let a = user(&core, "alice").await;
    let b = user(&core, "bob").await;
    let c = user(&core, "carol").await;
    let root = core.ensure_root(&a).await.unwrap();
    let proj = core.mkdir(&a, &root, "Project").await.unwrap();
    let file = core.write_bytes(&a, &proj.id, "brief.txt", b"hello", WriteOpts::user()).await.unwrap();

    // Not shared yet: invisible to bob.
    assert!(matches!(core.node_for(&b, &file.id, perm::READ).await, Err(Error::NotFound)));

    // Read-only share to bob.
    let s = core
        .create_share(
            &a,
            &proj.id,
            NewShare {
                kind: "user".into(),
                target: Some("bob".into()),
                permissions: Some(perm::READ),
                password: None,
                expires_at: None,
                label: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(core.resolve_path(&b, "/Project/brief.txt").await.unwrap().id, file.id);
    assert!(
        core.list_dir(&b, &core.ensure_root(&b).await.unwrap())
            .await
            .unwrap()
            .iter()
            .any(|e| e.node.name == "Project" && e.shared_by.is_some())
    );
    assert!(matches!(
        core.write_bytes(&b, &proj.id, "new.txt", b"x", WriteOpts::user()).await,
        Err(Error::Forbidden)
    ));
    assert!(matches!(core.delete(&b, &file.id).await, Err(Error::Forbidden)));
    assert_eq!(core.notifications(&b, true, 10).await.unwrap().len(), 1);

    // Upgrade to edit.
    core.update_share(
        &a,
        &s.share.id,
        ShareUpdate {
            permissions: Some(perm::ALL),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let bobs = core.write_bytes(&b, &proj.id, "from-bob.txt", b"hi", WriteOpts::user()).await.unwrap();
    assert_eq!(bobs.owner_id, a, "files created in a share belong to the share owner");
    assert_eq!(core.display_path(&b, &bobs).await.unwrap(), "/Project/from-bob.txt");

    // Group share to carol through a group.
    let g = core.create_group("friends").await.unwrap();
    core.add_to_group(&g.id, &c).await.unwrap();
    core.create_share(
        &a,
        &file.id,
        NewShare {
            kind: "group".into(),
            target: Some("friends".into()),
            permissions: None,
            password: None,
            expires_at: None,
            label: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(core.read_file(&c, &file.id).await.unwrap().1, b"hello");
    // Re-sharing cannot exceed what you have: carol has READ|UPDATE|SHARE on a file.
    let reshare = core
        .create_share(
            &c,
            &file.id,
            NewShare {
                kind: "link".into(),
                target: None,
                permissions: Some(perm::ALL),
                password: None,
                expires_at: None,
                label: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(reshare.share.permissions, perm::READ | perm::UPDATE);

    // Expired shares grant nothing.
    let s3 = core
        .create_share(
            &a,
            &file.id,
            NewShare {
                kind: "user".into(),
                target: Some("carol".into()),
                permissions: None,
                password: None,
                expires_at: Some(Utc::now() + Duration::hours(1)),
                label: None,
            },
        )
        .await
        .unwrap();
    sqlx::query("UPDATE shares SET expires_at = ? WHERE id = ?")
        .bind(Utc::now() - Duration::hours(1))
        .bind(&s3.share.id)
        .execute(&core.db)
        .await
        .unwrap();
    core.remove_from_group(&g.id, &c).await.unwrap();
    assert!(core.read_file(&c, &file.id).await.is_err());

    // Public link with password.
    let link = core
        .create_share(
            &a,
            &proj.id,
            NewShare {
                kind: "link".into(),
                target: None,
                permissions: None,
                password: Some("s3cret".into()),
                expires_at: None,
                label: Some("client".into()),
            },
        )
        .await
        .unwrap();
    assert!(link.has_password && link.url.is_some());
    let token = link.share.token.clone().unwrap();
    let share = core.link_share(&token).await.unwrap();
    assert!(!core.check_link_password(&share, "wrong") && core.check_link_password(&share, "s3cret"));
    let n = core.link_node(&share, "brief.txt").await.unwrap();
    assert_eq!(n.id, file.id);
    assert!(core.link_node(&share, "../x").await.is_err());
    assert_eq!(core.link_children(&core.link_node(&share, "").await.unwrap()).await.unwrap().len(), 2);
    core.delete_share(&a, &link.share.id).await.unwrap();
    assert!(core.link_share(&token).await.is_err());
}

#[tokio::test]
async fn talk_rooms_messages_calls() {
    let (core, _d) = core().await;
    let a = user(&core, "alice").await;
    let b = user(&core, "bob").await;
    let room = core.create_room(&a, "group", "Design", &["bob".into()]).await.unwrap();
    let alice = Actor::user(&a, "ALICE");
    let bob = Actor::user(&b, "BOB");

    let (bconn, mut rx) = core.hub.connect(&b);
    let m1 = core
        .post_message(
            &alice,
            &room.token,
            NewMessage {
                body: "hi @bob".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let ev = next(&mut rx).await;
    assert_eq!(ev["type"], "talk.message");
    assert_eq!(ev["message"]["body"], "hi @bob");

    let rooms = core.list_rooms(&b).await.unwrap();
    assert_eq!(rooms[0].unread, 1);
    let reply = core
        .post_message(
            &bob,
            &room.token,
            NewMessage {
                body: "hey".into(),
                reply_to: Some(m1.message.id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(reply.parent.as_ref().unwrap().body, "hi @bob");
    assert_eq!(core.list_rooms(&b).await.unwrap()[0].unread, 0, "posting marks earlier messages read");

    core.react(&bob, m1.message.id, "👍", true).await.unwrap();
    let v = core.react(&alice, m1.message.id, "👍", true).await.unwrap();
    assert_eq!(v.reactions["👍"].len(), 2);
    let edited = core.edit_message(&alice, m1.message.id, "hi @bob!").await.unwrap();
    assert!(edited.message.edited_at.is_some());
    assert!(core.edit_message(&bob, m1.message.id, "nope").await.is_err());

    let poll = core
        .create_poll(&alice, &room.token, "Lunch?", &["Pizza".into(), "Sushi".into()], false)
        .await
        .unwrap();
    let voted = core.vote(&bob, poll.message.id, &[1]).await.unwrap();
    assert_eq!(voted.message.meta.0["votes"][&b], serde_json::json!([1]));
    assert!(core.vote(&bob, poll.message.id, &[0, 1]).await.is_err());

    let msgs = core.list_messages(&bob, &room.token, None, None, 50).await.unwrap();
    assert!(msgs.iter().any(|m| m.message.kind == "system"));
    let page = core.list_messages(&bob, &room.token, Some(reply.message.id), None, 1).await.unwrap();
    assert_eq!(page.len(), 1);
    assert!(page[0].message.id < reply.message.id);

    // Outsiders can't read.
    let c = user(&core, "carol").await;
    assert!(core.list_messages(&Actor::user(&c, "C"), &room.token, None, None, 10).await.is_err());

    // Direct rooms are unique per pair.
    let d1 = core.direct_room(&a, "bob").await.unwrap();
    let d2 = core.direct_room(&b, "alice").await.unwrap();
    assert_eq!(d1.id, d2.id);
    assert_eq!(core.room_display_name(&d1, &a).await.unwrap(), "BOB");

    // Sharing a file into the room grants participants access.
    let root = core.ensure_root(&a).await.unwrap();
    let f = core.write_bytes(&a, &root, "spec.pdf", b"%PDF", WriteOpts::user()).await.unwrap();
    let fm = core.share_file_to_room(&a, &room.token, &f.id, "").await.unwrap();
    assert_eq!(fm.message.kind, "file");
    assert_eq!(core.read_file(&b, &f.id).await.unwrap().1, b"%PDF");

    // Search finds messages for participants only.
    let hits = core.search(&b, "lunch", None, 10).await.unwrap();
    assert!(hits.iter().any(|h| h.kind == "message"));
    assert!(core.search(&c, "lunch", None, 10).await.unwrap().is_empty());

    // Calls: join, signal, leave.
    let (s1, others, _) = core.join_call(&alice, &room.token, 1, true, true).await.unwrap();
    assert!(others.is_empty());
    let (s2, others, _) = core.join_call(&bob, &room.token, bconn, true, false).await.unwrap();
    assert_eq!(others.len(), 1);
    assert!(core.owns_session(&room.id, &s2, &b, bconn) && !core.owns_session(&room.id, &s2, &a, bconn));
    while rx.try_recv().is_ok() {}
    core.relay_signal(&room.id, &s1, &s2, serde_json::json!({"sdp": "offer"})).unwrap();
    let sig = next(&mut rx).await;
    assert_eq!(sig["type"], "call.signal");
    assert_eq!(sig["payload"]["sdp"], "offer");
    core.leave_calls_of_connection(&b, bconn).await;
    assert_eq!(core.call_members(&room.id).len(), 1);
    core.leave_call(&room.id, &s1).await.unwrap();
    assert!(core.call_members(&room.id).is_empty());

    // Guests in public rooms.
    let public = core.create_room(&a, "public", "Open house", &[]).await.unwrap();
    let (guest, _, secret) = core.create_guest(&public.token, "Dana").await.unwrap();
    let (g2, _) = core.resolve_guest(&secret).await.unwrap();
    assert_eq!(guest, g2);
    core.post_message(
        &guest,
        &public.token,
        NewMessage {
            body: "hello from a guest".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(core.create_guest(&room.token, "Eve").await.is_err(), "group rooms reject guests");
}

#[tokio::test]
async fn boards_and_cards() {
    let (core, _d) = core().await;
    let a = user(&core, "alice").await;
    let b = user(&core, "bob").await;
    let board = core.create_board(&a, "Launch", None, None).await.unwrap();
    let view = core.board_view(&a, &board.id).await.unwrap();
    assert_eq!(view.stacks.len(), 3);
    let todo = &view.stacks[0].stack.id;
    let done = &view.stacks[2].stack.id;
    let c1 = core.create_card(&a, todo, "Write press release", "draft **v1**", None).await.unwrap();
    let c2 = core
        .create_card(&a, todo, "Book venue", "", Some(Utc::now() + Duration::days(3)))
        .await
        .unwrap();
    assert!(core.board_view(&b, &board.id).await.is_err());
    core.add_board_member(&a, &board.id, "bob", true).await.unwrap();
    core.set_card_assignee(&a, &c2.id, "bob", true).await.unwrap();
    assert_eq!(core.my_cards(&b).await.unwrap()[0].id, c2.id);
    core.move_card(&b, &c2.id, done, 0).await.unwrap();
    core.move_card(&b, &c1.id, todo, 5).await.unwrap();
    let label = view.labels[0].id.clone();
    core.set_card_label(&a, &c1.id, &label, true).await.unwrap();
    let root = core.ensure_root(&a).await.unwrap();
    let mm = core.create_document(&a, &root, "Ideas", DocKind::Mindmap).await.unwrap();
    core.add_card_link(&a, &c1.id, "file", &mm.id, None).await.unwrap();
    core.add_card_link(&a, &c1.id, "url", "https://example.com", Some("ref")).await.unwrap();
    assert!(core.add_card_link(&a, &c1.id, "url", "javascript:alert(1)", None).await.is_err());
    core.add_card_comment(&b, &c1.id, "looks good").await.unwrap();
    let v = core.board_view(&b, &board.id).await.unwrap();
    let card = v.stacks[0].cards.iter().find(|c| c.card.id == c1.id).unwrap();
    assert_eq!(card.labels, vec![label]);
    assert_eq!(card.links.len(), 2);
    assert_eq!(card.comment_count, 1);
    assert_eq!(v.stacks[2].cards[0].card.id, c2.id);
    assert!(core.search(&b, "press", Some(&["card"]), 5).await.unwrap().len() == 1);
    // Read-only members can't edit.
    core.add_board_member(&a, &board.id, "bob", false).await.unwrap();
    assert!(matches!(core.create_card(&b, todo, "x", "", None).await, Err(Error::Forbidden)));
}

#[tokio::test]
async fn live_documents_edit_and_persist() {
    let (core, _d) = core().await;
    let a = user(&core, "alice").await;
    let b = user(&core, "bob").await;
    let root = core.ensure_root(&a).await.unwrap();
    let doc = core.create_document(&a, &root, "Notes", DocKind::Text).await.unwrap();
    assert_eq!(doc.name, "Notes.md");

    // A connected editor sees the server-side edit and the file is written back.
    let (mut conn, initial) = core.collab_open(&a, &doc.id).await.unwrap();
    assert!(!initial.is_empty());
    core.collab_edit(&a, &doc.id, |d| {
        let t = d.get_or_insert_text("content");
        let mut txn = d.transact_mut();
        let len = t.get_string(&txn).len() as u32;
        t.insert(&mut txn, len, "Added by the assistant.\n");
        Ok(())
    })
    .await
    .unwrap();
    let frame = conn.rx.recv().await.unwrap();
    assert!(!frame.is_empty());
    assert_eq!(
        String::from_utf8(core.collab_content(&a, &doc.id).await.unwrap()).unwrap(),
        "# Notes\n\nAdded by the assistant.\n"
    );
    assert_eq!(
        String::from_utf8(core.read_file(&a, &doc.id).await.unwrap().1).unwrap(),
        "# Notes\n\nAdded by the assistant.\n"
    );

    // A real Yjs-style client update applied through the protocol.
    let client = yrs::Doc::new();
    {
        let t = client.get_or_insert_text("content");
        let mut txn = client.transact_mut();
        t.insert(&mut txn, 0, "x");
    }
    let sv_frame = {
        use yrs::ReadTxn;
        use yrs::sync::{Message, SyncMessage};
        use yrs::updates::encoder::Encode;
        Message::Sync(SyncMessage::SyncStep1(client.transact().state_vector())).encode_v1()
    };
    let replies = core.collab_receive(&mut conn, &sv_frame).unwrap();
    assert_eq!(replies.len(), 1, "server answers sync step 1 with step 2");

    // Read-only viewers can't change the document.
    let ro = core
        .create_share(
            &a,
            &doc.id,
            NewShare {
                kind: "user".into(),
                target: Some("bob".into()),
                permissions: Some(perm::READ),
                password: None,
                expires_at: None,
                label: None,
            },
        )
        .await;
    assert!(ro.is_ok());
    let (bconn, _) = core.collab_open(&b, &doc.id).await.unwrap();
    assert!(bconn.read_only);
    assert!(core.collab_edit(&b, &doc.id, |_| Ok(())).await.is_err());
    core.collab_close(bconn).await;
    core.collab_close(conn).await;

    // Mind map editing through the CRDT reaches the JSON file.
    let mm = core.create_document(&a, &root, "Roadmap", DocKind::Mindmap).await.unwrap();
    core.collab_edit(&a, &mm.id, |d| {
        let nodes = d.get_or_insert_map("nodes");
        let mut txn = d.transact_mut();
        let obj = serde_json::json!({"parent": "root", "text": "Q1", "order": 1});
        ferrite_core::collab::codec::put_object(&mut txn, &nodes, "q1", obj.as_object().unwrap());
        Ok(())
    })
    .await
    .unwrap();
    let saved: Value = serde_json::from_slice(&core.read_file(&a, &mm.id).await.unwrap().1).unwrap();
    assert_eq!(ferrite_core::collab::codec::mindmap_outline(&saved), "- Roadmap\n  - Q1\n");
    // Versions were kept for the first live save.
    assert!(!core.list_versions(&a, &mm.id).await.unwrap().is_empty());

    // External writes reset live sessions.
    let (mut c2, _) = core.collab_open(&a, &mm.id).await.unwrap();
    core.write_bytes(&a, &root, "Roadmap.fmind", b"{\"nodes\":[]}", WriteOpts::user())
        .await
        .unwrap();
    let reset = c2.rx.recv().await.unwrap();
    assert!(ferrite_core::collab::is_reset(&reset));
    core.collab_close(c2).await;
    assert_eq!(nodes_count(&core, &a, &mm.id).await, 0, "file content wins after an external write");
}

async fn nodes_count(core: &Core, user: &str, id: &str) -> usize {
    let v: Value = serde_json::from_slice(&core.collab_content(user, id).await.unwrap()).unwrap();
    v["nodes"].as_array().map(|a| a.len()).unwrap_or(0)
}

#[tokio::test]
async fn user_deletion_releases_storage() {
    let (core, _d) = core().await;
    let a = user(&core, "alice").await;
    let root = core.ensure_root(&a).await.unwrap();
    core.write_bytes(&a, &root, "a.txt", b"one", WriteOpts::user()).await.unwrap();
    core.write_bytes(&a, &root, "a.txt", b"two", WriteOpts::user()).await.unwrap();
    core.delete_user(&a).await.unwrap();
    let blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs").fetch_one(&core.db).await.unwrap();
    assert_eq!(blobs, 0);
}
