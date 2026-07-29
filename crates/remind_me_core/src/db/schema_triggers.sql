-- GENERATED from remind_me's schema. Do not hand-edit.
-- Regenerate by dumping sqlite_master from a reference database.

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
            INSERT INTO memories_fts(memories_fts, rowid, content, category, tags)
            VALUES ('delete', old.rowid, old.content, old.category, old.tags);
        END;

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
            INSERT INTO memories_fts(rowid, content, category, tags)
            VALUES (new.rowid, new.content, new.category, new.tags);
        END;

CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
            INSERT INTO memories_fts(memories_fts, rowid, content, category, tags)
            VALUES ('delete', old.rowid, old.content, old.category, old.tags);
            INSERT INTO memories_fts(rowid, content, category, tags)
            VALUES (new.rowid, new.content, new.category, new.tags);
        END;

CREATE TRIGGER IF NOT EXISTS memories_outbox_ai
        AFTER INSERT ON memories BEGIN
            INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
            VALUES (
                NEW.id, 'insert',
                json_object(
                    'id',                 NEW.id,
                    'content',            NEW.content,
                    'category',           NEW.category,
                    'tags',               NEW.tags,
                    'source',             NEW.source,
                    'metadata',           NEW.metadata,
                    'created_at',         NEW.created_at,
                    'updated_at',         NEW.updated_at,
                    'capture_id',         NEW.capture_id,
                    'node_id',            NEW.node_id,
                    'client',             NEW.client,
                    'accessed_at',        NEW.accessed_at,
                    'access_count',       NEW.access_count,
                    'decay_rate',         NEW.decay_rate,
                    'vitality',           NEW.vitality,
                    'base_weight',        NEW.base_weight,
                    'status',             NEW.status,
                    'memory_type',        NEW.memory_type,
                    'source_capture_id',  NEW.source_capture_id,
                    'subject',            NEW.subject,
                    'predicate',          NEW.predicate,
                    'object',             NEW.object,
                    'superseded_by',      NEW.superseded_by
                ),
                datetime('now', 'utc')
            );
        END;

CREATE TRIGGER IF NOT EXISTS memories_outbox_au
        AFTER UPDATE ON memories BEGIN
            INSERT INTO sync_outbox (memory_id, operation, payload, created_at)
            VALUES (
                NEW.id, 'update',
                json_object(
                    'id',                 NEW.id,
                    'content',            NEW.content,
                    'category',           NEW.category,
                    'tags',               NEW.tags,
                    'source',             NEW.source,
                    'metadata',           NEW.metadata,
                    'created_at',         NEW.created_at,
                    'updated_at',         NEW.updated_at,
                    'capture_id',         NEW.capture_id,
                    'node_id',            NEW.node_id,
                    'client',             NEW.client,
                    'accessed_at',        NEW.accessed_at,
                    'access_count',       NEW.access_count,
                    'decay_rate',         NEW.decay_rate,
                    'vitality',           NEW.vitality,
                    'base_weight',        NEW.base_weight,
                    'status',             NEW.status,
                    'memory_type',        NEW.memory_type,
                    'source_capture_id',  NEW.source_capture_id,
                    'subject',            NEW.subject,
                    'predicate',          NEW.predicate,
                    'object',             NEW.object,
                    'superseded_by',      NEW.superseded_by
                ),
                datetime('now', 'utc')
            );
        END;

CREATE TRIGGER IF NOT EXISTS memories_tags_ad AFTER DELETE ON memories
        BEGIN
            DELETE FROM memory_tags WHERE memory_id = OLD.id;
        END;

CREATE TRIGGER IF NOT EXISTS memories_tags_ai AFTER INSERT ON memories
        BEGIN
            INSERT OR IGNORE INTO memory_tags (memory_id, tag)
            SELECT NEW.id, je.value
              FROM json_each(NEW.tags) AS je
             WHERE typeof(je.value) = 'text'
               AND json_valid(NEW.tags);
        END;

CREATE TRIGGER IF NOT EXISTS memories_tags_au AFTER UPDATE OF tags ON memories
        BEGIN
            DELETE FROM memory_tags WHERE memory_id = OLD.id;
            INSERT OR IGNORE INTO memory_tags (memory_id, tag)
            SELECT NEW.id, je.value
              FROM json_each(NEW.tags) AS je
             WHERE typeof(je.value) = 'text'
               AND json_valid(NEW.tags);
        END;

CREATE TRIGGER IF NOT EXISTS wiki_pages_ad AFTER DELETE ON wiki_pages BEGIN
            INSERT INTO wiki_fts(wiki_fts, rowid, title, content)
            VALUES ('delete', old.rowid, old.title, old.content);
        END;

CREATE TRIGGER IF NOT EXISTS wiki_pages_ai AFTER INSERT ON wiki_pages BEGIN
            INSERT INTO wiki_fts(rowid, title, content)
            VALUES (new.rowid, new.title, new.content);
        END;

CREATE TRIGGER IF NOT EXISTS wiki_pages_au AFTER UPDATE ON wiki_pages BEGIN
            INSERT INTO wiki_fts(wiki_fts, rowid, title, content)
            VALUES ('delete', old.rowid, old.title, old.content);
            INSERT INTO wiki_fts(rowid, title, content)
            VALUES (new.rowid, new.title, new.content);
        END;
