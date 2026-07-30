-- GENERATED from remind_me's schema. Do not hand-edit.
-- Regenerate by dumping sqlite_master from a reference database.

CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(kind);

CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(name);

CREATE INDEX IF NOT EXISTS idx_entities_updated_at ON entities(updated_at);

CREATE INDEX IF NOT EXISTS idx_entity_relations_created_at
            ON entity_relations(created_at);

CREATE INDEX IF NOT EXISTS idx_entity_relations_object
            ON entity_relations(object_entity_id);

CREATE INDEX IF NOT EXISTS idx_entity_relations_subject
            ON entity_relations(subject_entity_id);

CREATE INDEX IF NOT EXISTS idx_memories_capture_id ON memories(capture_id);

CREATE INDEX IF NOT EXISTS idx_memories_category ON memories(category);

CREATE INDEX IF NOT EXISTS idx_memories_client ON memories(client);

CREATE INDEX IF NOT EXISTS idx_memories_created ON memories(created_at);

CREATE INDEX IF NOT EXISTS idx_memories_deleted_at ON memories(deleted_at);

CREATE INDEX IF NOT EXISTS idx_memories_doc_chunk ON memories(doc_id, chunk_index);

CREATE INDEX IF NOT EXISTS idx_memories_memory_type ON memories(memory_type);

CREATE INDEX IF NOT EXISTS idx_memories_source ON memories(source);

CREATE INDEX IF NOT EXISTS idx_memories_source_capture_id ON memories(source_capture_id);

CREATE INDEX IF NOT EXISTS idx_memories_status ON memories(status);

CREATE INDEX IF NOT EXISTS idx_memories_subject ON memories(subject);

CREATE INDEX IF NOT EXISTS idx_memories_updated_at
            ON memories(updated_at);

CREATE INDEX IF NOT EXISTS idx_memories_vitality ON memories(vitality);

CREATE INDEX IF NOT EXISTS idx_memory_associations_a
            ON memory_associations(memory_id_a);

CREATE INDEX IF NOT EXISTS idx_memory_associations_b
            ON memory_associations(memory_id_b);

CREATE INDEX IF NOT EXISTS idx_memory_entities_created_at
            ON memory_entities(created_at);

CREATE INDEX IF NOT EXISTS idx_memory_entities_entity
            ON memory_entities(entity_id);

CREATE INDEX IF NOT EXISTS idx_memory_feedback_memory_id
            ON memory_feedback(memory_id);

CREATE INDEX IF NOT EXISTS idx_memory_tags_tag ON memory_tags(tag);

CREATE INDEX IF NOT EXISTS idx_outbox_created_at
            ON sync_outbox(created_at);

CREATE INDEX IF NOT EXISTS idx_outbox_memory_id
            ON sync_outbox(memory_id);

CREATE INDEX IF NOT EXISTS idx_outbox_unsent
            ON sync_outbox(sent_at) WHERE sent_at = '';

CREATE INDEX IF NOT EXISTS idx_vec_chunks_memory ON vec_chunks(memory_rowid);

CREATE INDEX IF NOT EXISTS idx_wiki_links_dst ON wiki_links(dst_slug);
