//! Coverage for `remind_me_wiki_list` / `remind_me_wiki_delete`.

use remind_me_core::wiki::{
    delete_wiki_page, get_wiki_page, list_wiki_pages, write_wiki_page, WikiDeleteOutcome,
};
use remind_me_core::Database;

#[test]
fn list_is_empty_on_a_fresh_wiki() {
    let db = Database::open_in_memory().unwrap();
    assert!(list_wiki_pages(&db.conn()).unwrap().is_empty());
}

#[test]
fn list_returns_pages_most_recently_updated_first() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "first", "First", "body", "general").unwrap();
    write_wiki_page(&conn, "second", "Second", "body", "general").unwrap();
    // Touch the older page so it becomes the most recent.
    write_wiki_page(&conn, "first", "First", "revised", "general").unwrap();

    let slugs: Vec<String> = list_wiki_pages(&conn)
        .unwrap()
        .into_iter()
        .map(|p| p.slug)
        .collect();
    assert_eq!(slugs.len(), 2);
    assert_eq!(slugs[0], "first", "most recently updated sorts first");
}

#[test]
fn delete_resolves_by_slug() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "vlan-setup", "VLAN Setup", "body", "network").unwrap();

    assert_eq!(
        delete_wiki_page(&conn, "vlan-setup").unwrap(),
        WikiDeleteOutcome::Deleted
    );
    assert!(get_wiki_page(&conn, "vlan-setup").unwrap().is_none());
}

#[test]
fn delete_resolves_by_title() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "vlan-setup", "VLAN Setup", "body", "network").unwrap();

    // The human title, not the slug — this is the case the reference supports
    // by running the input through slugify().
    assert_eq!(
        delete_wiki_page(&conn, "VLAN Setup").unwrap(),
        WikiDeleteOutcome::Deleted
    );
    assert!(get_wiki_page(&conn, "vlan-setup").unwrap().is_none());
}

#[test]
fn delete_tolerates_casing_and_punctuation_drift_in_the_title() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    for probe in ["VLAN  Setup!", "vlan setup", "  VLAN-Setup  "] {
        write_wiki_page(&conn, "vlan-setup", "VLAN Setup", "body", "network").unwrap();
        assert_eq!(
            delete_wiki_page(&conn, probe).unwrap(),
            WikiDeleteOutcome::Deleted,
            "{:?} should resolve to the vlan-setup slug",
            probe
        );
    }
}

#[test]
fn delete_reports_missing_pages_without_touching_others() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "keeper", "Keeper", "body", "general").unwrap();

    assert_eq!(
        delete_wiki_page(&conn, "no-such-page").unwrap(),
        WikiDeleteOutcome::NotFound
    );
    assert_eq!(list_wiki_pages(&conn).unwrap().len(), 1);
}

#[test]
fn delete_is_idempotent() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "ephemeral", "Ephemeral", "body", "general").unwrap();

    assert_eq!(
        delete_wiki_page(&conn, "ephemeral").unwrap(),
        WikiDeleteOutcome::Deleted
    );
    assert_eq!(
        delete_wiki_page(&conn, "ephemeral").unwrap(),
        WikiDeleteOutcome::NotFound,
        "a second delete is a no-op, not an error"
    );
}

#[test]
fn delete_refuses_reserved_system_pages() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();

    for reserved in ["index", "log", "schema"] {
        write_wiki_page(&conn, reserved, reserved, "body", "system").unwrap();
        assert_eq!(
            delete_wiki_page(&conn, reserved).unwrap(),
            WikiDeleteOutcome::Reserved,
            "{} must be protected",
            reserved
        );
        assert!(
            get_wiki_page(&conn, reserved).unwrap().is_some(),
            "{} must survive the refused delete",
            reserved
        );
    }
}

#[test]
fn delete_refuses_reserved_pages_addressed_by_title() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    write_wiki_page(&conn, "index", "Index", "body", "system").unwrap();

    // "Index" slugifies to "index", so the guard must catch it here too —
    // checking the raw input instead of the slug would let this through.
    assert_eq!(
        delete_wiki_page(&conn, "Index").unwrap(),
        WikiDeleteOutcome::Reserved
    );
    assert!(get_wiki_page(&conn, "index").unwrap().is_some());
}
