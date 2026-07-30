//! Coverage for vector storage, brute-force semantic search, and
//! `remind_me_reindex`.
//!
//! A [`FakeEmbedder`] stands in for the real Ollama backend throughout: it
//! returns pre-defined vectors for known input strings, so a test can assert
//! an exact similarity ordering without a real embedding model or network
//! access. `OllamaEmbedder` itself — the HTTP client — has its own coverage
//! in `ollama_embedder_test.rs`, against a fake HTTP server.

use remind_me_core::db::queries;
use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder};
use remind_me_core::vectors::{
    delete_chunks_for_memory, dimension_of, embed_and_store, reindex, reindex_with, semantic_search,
};
use remind_me_core::{Database, MemoryAddInput};
use rusqlite::Connection;
use std::collections::HashMap;

/// Returns a pre-defined vector for each known input string, verbatim (no
/// chunking-awareness needed as long as test content stays under
/// `EMBED_CHUNK_CHARS`, so `chunk_text` never splits it).
struct FakeEmbedder {
    dim: usize,
    vectors: HashMap<String, Vec<f32>>,
}

impl FakeEmbedder {
    fn new(dim: usize) -> Self {
        Self {
            dim,
            vectors: HashMap::new(),
        }
    }

    fn with(mut self, text: &str, vector: Vec<f32>) -> Self {
        assert_eq!(
            vector.len(),
            self.dim,
            "fixture vector length must match the embedder's dim"
        );
        self.vectors.insert(text.to_string(), vector);
        self
    }
}

impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String], _role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        texts
            .iter()
            .map(|t| {
                self.vectors
                    .get(t)
                    .cloned()
                    .ok_or_else(|| EmbedError(format!("FakeEmbedder has no vector for {:?}", t)))
            })
            .collect()
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

fn add(conn: &Connection, content: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: "general".into(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap()
    .id
}

fn add_with_category(conn: &Connection, content: &str, category: &str) -> String {
    queries::add_memory(
        conn,
        MemoryAddInput {
            content: content.to_string(),
            category: category.to_string(),
            tags: vec![],
            source: "manual".into(),
            metadata: serde_json::json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
        },
    )
    .unwrap()
    .id
}

fn chunk_count(conn: &Connection, memory_id: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM vec_chunks vc
           JOIN memories m ON m.rowid = vc.memory_rowid
          WHERE m.id = ?",
        [memory_id],
        |r| r.get(0),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Vector round-trip and dimension inference
// ---------------------------------------------------------------------------

#[test]
fn embedding_a_memory_stores_one_chunk_and_dimension_infers_correctly() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "quokkas live on Rottnest Island");
    let embedder =
        FakeEmbedder::new(4).with("quokkas live on Rottnest Island", vec![1.0, 0.0, 0.0, 0.0]);

    let chunks = embed_and_store(&conn, &embedder, &id, "quokkas live on Rottnest Island").unwrap();

    assert_eq!(chunks, 1);
    assert_eq!(chunk_count(&conn, &id), 1);

    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT ve.embedding FROM vec_chunks vc
               JOIN vec_embeddings ve ON ve.vec_rowid = vc.vec_rowid
               JOIN memories m ON m.rowid = vc.memory_rowid
              WHERE m.id = ?",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        dimension_of(&bytes),
        4,
        "384/768/1024 all round-trip the same way"
    );
    assert_eq!(bytes.len(), 16, "4 floats * 4 bytes");
}

#[test]
fn re_embedding_replaces_rather_than_accumulates_chunks() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "original text");
    let embedder = FakeEmbedder::new(2)
        .with("original text", vec![1.0, 0.0])
        .with("revised text", vec![0.0, 1.0]);

    embed_and_store(&conn, &embedder, &id, "original text").unwrap();
    assert_eq!(chunk_count(&conn, &id), 1);

    embed_and_store(&conn, &embedder, &id, "revised text").unwrap();

    assert_eq!(
        chunk_count(&conn, &id),
        1,
        "not 2 -- the old chunk was replaced"
    );
}

#[test]
fn embedding_blank_content_stores_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "placeholder");
    let embedder = FakeEmbedder::new(2);

    let chunks = embed_and_store(&conn, &embedder, &id, "   ").unwrap();

    assert_eq!(chunks, 0);
    assert_eq!(chunk_count(&conn, &id), 0);
}

#[test]
fn embedding_an_unknown_memory_id_is_a_silent_no_op() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let embedder = FakeEmbedder::new(2).with("text", vec![1.0, 0.0]);

    let chunks = embed_and_store(&conn, &embedder, "mem_ghost", "text").unwrap();

    assert_eq!(chunks, 0);
}

// ---------------------------------------------------------------------------
// Deletion cleans up chunks (rowid-reuse safety)
// ---------------------------------------------------------------------------

#[test]
fn deleting_a_memory_removes_its_chunks_and_embeddings() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "doomed content");
    let embedder = FakeEmbedder::new(2).with("doomed content", vec![1.0, 0.0]);
    embed_and_store(&conn, &embedder, &id, "doomed content").unwrap();
    assert_eq!(chunk_count(&conn, &id), 1);

    queries::delete_memory(&conn, &id).unwrap();

    let remaining_chunks: i64 = conn
        .query_row("SELECT count(*) FROM vec_chunks", [], |r| r.get(0))
        .unwrap();
    let remaining_vectors: i64 = conn
        .query_row("SELECT count(*) FROM vec_embeddings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        remaining_chunks, 0,
        "a reused rowid must not inherit this memory's chunks"
    );
    assert_eq!(remaining_vectors, 0);
}

#[test]
fn a_deleted_memorys_rowid_does_not_leak_stale_embeddings_to_its_successor() {
    // The scenario delete_chunks_for_memory exists to prevent: SQLite reuses
    // freed rowids, so without cleanup, a brand-new memory landing on the
    // same rowid would silently "own" the deleted memory's vectors.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let first = add(&conn, "first memory");
    let embedder = FakeEmbedder::new(2)
        .with("first memory", vec![1.0, 0.0])
        .with("second memory", vec![0.0, 1.0]);
    embed_and_store(&conn, &embedder, &first, "first memory").unwrap();
    queries::delete_memory(&conn, &first).unwrap();

    let second = add(&conn, "second memory");
    // The new memory is not embedded yet -- confirm it inherited nothing.
    assert_eq!(chunk_count(&conn, &second), 0);

    embed_and_store(&conn, &embedder, &second, "second memory").unwrap();
    assert_eq!(chunk_count(&conn, &second), 1);
}

// ---------------------------------------------------------------------------
// Direct delete_chunks_for_memory
// ---------------------------------------------------------------------------

#[test]
fn delete_chunks_for_memory_is_a_no_op_on_a_never_embedded_memory() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "never embedded");
    let rowid: i64 = conn
        .query_row("SELECT rowid FROM memories WHERE id = ?", [&id], |r| {
            r.get(0)
        })
        .unwrap();

    let removed = delete_chunks_for_memory(&conn, rowid).unwrap();

    assert_eq!(removed, 0);
}

// ---------------------------------------------------------------------------
// Semantic search
// ---------------------------------------------------------------------------

#[test]
fn semantic_search_ranks_by_similarity_to_the_query() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let close = add(&conn, "quokkas are marsupials found on Rottnest Island");
    let far = add(&conn, "the deploy window is Tuesdays at 2pm");

    let embedder = FakeEmbedder::new(3)
        .with(
            "quokkas are marsupials found on Rottnest Island",
            vec![1.0, 0.0, 0.0],
        )
        .with("the deploy window is Tuesdays at 2pm", vec![0.0, 1.0, 0.0])
        .with("tell me about quokkas", vec![0.9, 0.1, 0.0]);
    embed_and_store(
        &conn,
        &embedder,
        &close,
        "quokkas are marsupials found on Rottnest Island",
    )
    .unwrap();
    embed_and_store(
        &conn,
        &embedder,
        &far,
        "the deploy window is Tuesdays at 2pm",
    )
    .unwrap();

    let results = semantic_search(&conn, &embedder, "tell me about quokkas", 10, None).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, close, "the near-parallel vector ranks first");
    assert_eq!(results[1].id, far);
}

#[test]
fn semantic_search_keeps_only_a_memorys_single_best_chunk() {
    // A memory that happens to own several chunks must not out-rank one
    // with a single chunk purely for having more shots at matching.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let multi = add(&conn, "multi chunk memory");
    let single = add(&conn, "single chunk memory");
    let embedder = FakeEmbedder::new(2)
        .with("multi chunk memory", vec![0.1, 0.0])
        .with("single chunk memory", vec![0.9, 0.0])
        .with("query text", vec![1.0, 0.0]);
    embed_and_store(&conn, &embedder, &multi, "multi chunk memory").unwrap();
    embed_and_store(&conn, &embedder, &single, "single chunk memory").unwrap();

    let results = semantic_search(&conn, &embedder, "query text", 10, None).unwrap();

    assert_eq!(results[0].id, single, "0.9 similarity beats 0.1");
}

#[test]
fn semantic_search_excludes_superseded_and_deleted_memories() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let stale = add(&conn, "stale content");
    let embedder = FakeEmbedder::new(2)
        .with("stale content", vec![1.0, 0.0])
        .with("removed content", vec![1.0, 0.0])
        .with("query", vec![1.0, 0.0]);
    embed_and_store(&conn, &embedder, &stale, "stale content").unwrap();
    conn.execute(
        "UPDATE memories SET superseded_by = 'mem_new' WHERE id = ?",
        [&stale],
    )
    .unwrap();

    let removed = add(&conn, "removed content");
    embed_and_store(&conn, &embedder, &removed, "removed content").unwrap();
    conn.execute(
        "UPDATE memories SET deleted_at = '2026-01-01T00:00:00Z' WHERE id = ?",
        [&removed],
    )
    .unwrap();

    let results = semantic_search(&conn, &embedder, "query", 10, None).unwrap();
    assert!(results.is_empty());
}

#[test]
fn semantic_search_filters_by_category() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let wildlife = add_with_category(&conn, "quokka content", "wildlife");
    let general = add_with_category(&conn, "general content", "general");
    let embedder = FakeEmbedder::new(2)
        .with("quokka content", vec![1.0, 0.0])
        .with("general content", vec![1.0, 0.0])
        .with("query", vec![1.0, 0.0]);
    embed_and_store(&conn, &embedder, &wildlife, "quokka content").unwrap();
    embed_and_store(&conn, &embedder, &general, "general content").unwrap();

    let results = semantic_search(&conn, &embedder, "query", 10, Some("wildlife")).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, wildlife);
}

#[test]
fn semantic_search_over_an_empty_store_returns_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let embedder = FakeEmbedder::new(2).with("query", vec![1.0, 0.0]);

    let results = semantic_search(&conn, &embedder, "query", 10, None).unwrap();

    assert!(results.is_empty());
}

#[test]
fn a_stale_dimension_vector_is_skipped_rather_than_crashing_the_scan() {
    // As if REMIND_ME_EMBEDDING_DIM changed and old vectors were never
    // reindexed: a leftover vector at the wrong width must not panic the
    // dot product, and must not be treated as a match either.
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "old width content");
    let rowid: i64 = conn
        .query_row("SELECT rowid FROM memories WHERE id = ?", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    conn.execute(
        "INSERT INTO vec_chunks (memory_rowid, chunk_ix) VALUES (?, 0)",
        [rowid],
    )
    .unwrap();
    let vec_rowid = conn.last_insert_rowid();
    // 3 floats (12 bytes) where the query embedder produces 2.
    conn.execute(
        "INSERT INTO vec_embeddings (vec_rowid, embedding) VALUES (?, ?)",
        rusqlite::params![vec_rowid, vec![0u8; 12]],
    )
    .unwrap();

    let embedder = FakeEmbedder::new(2).with("query", vec![1.0, 0.0]);
    let results = semantic_search(&conn, &embedder, "query", 10, None).unwrap();

    assert!(
        results.is_empty(),
        "the mismatched-width vector is skipped, not matched"
    );
}

// ---------------------------------------------------------------------------
// Reindex
// ---------------------------------------------------------------------------

#[test]
fn reindex_reports_degraded_with_no_embedder_configured() {
    // Deliberately does not touch REMIND_ME_EMBEDDING_BACKEND: this asserts
    // the default (unset) case, which every other test in this crate's
    // suite already relies on being the ambient state.
    std::env::remove_var(remind_me_core::embedder::EMBEDDING_BACKEND_ENV);
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    add(&conn, "never embedded");

    let result = reindex(&conn).unwrap();

    assert!(result.degraded);
    assert_eq!(result.embedded, 0);
}

#[test]
fn reindex_with_embeds_only_memories_missing_a_vector() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let already_embedded = add(&conn, "already embedded");
    let missing = add(&conn, "missing content");
    let embedder = FakeEmbedder::new(2)
        .with("already embedded", vec![1.0, 0.0])
        .with("missing content", vec![0.0, 1.0]);
    embed_and_store(&conn, &embedder, &already_embedded, "already embedded").unwrap();

    let result = reindex_with(&conn, &embedder).unwrap();

    assert!(!result.degraded);
    assert_eq!(
        result.missing, 1,
        "only the never-embedded memory counts as missing"
    );
    assert_eq!(result.embedded, 1);
    assert_eq!(result.chunks_created, 1);
    assert_eq!(chunk_count(&conn, &missing), 1);
}

#[test]
fn reindex_with_is_idempotent_and_preserves_existing_embeddings() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let id = add(&conn, "some content");
    let embedder = FakeEmbedder::new(2).with("some content", vec![1.0, 0.0]);

    let first = reindex_with(&conn, &embedder).unwrap();
    assert_eq!(first.embedded, 1);

    let second = reindex_with(&conn, &embedder).unwrap();

    assert_eq!(
        second.missing, 0,
        "nothing left unembedded on the second pass"
    );
    assert_eq!(second.embedded, 0);
    assert_eq!(second.chunks_created, 0);
    assert_eq!(
        chunk_count(&conn, &id),
        1,
        "the original chunk was left alone, not duplicated"
    );
}

#[test]
fn reindex_with_over_an_empty_store_does_nothing() {
    let db = Database::open_in_memory().unwrap();
    let conn = db.conn();
    let embedder = FakeEmbedder::new(2);

    let result = reindex_with(&conn, &embedder).unwrap();

    assert!(!result.degraded);
    assert_eq!(result.missing, 0);
    assert_eq!(result.embedded, 0);
    assert_eq!(result.chunks_created, 0);
}
