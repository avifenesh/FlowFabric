-- RFC-025 Phase 6 follow-up — mirror PG 0022: add event_kind to the
-- primary key so same-ms register→heartbeat on boot doesn't collide.
--
-- SQLite doesn't support ALTER TABLE ... DROP CONSTRAINT. Use the
-- canonical rename + copy pattern.

CREATE TABLE ff_worker_registry_event_new (
    namespace              TEXT    NOT NULL,
    worker_instance_id     TEXT    NOT NULL,
    event_kind             TEXT    NOT NULL,
    event_at_ms            INTEGER NOT NULL,
    reason                 TEXT    NULL,
    PRIMARY KEY (namespace, worker_instance_id, event_at_ms, event_kind)
);

INSERT INTO ff_worker_registry_event_new
  (namespace, worker_instance_id, event_kind, event_at_ms, reason)
  SELECT namespace, worker_instance_id, event_kind, event_at_ms, reason
  FROM ff_worker_registry_event;

DROP TABLE ff_worker_registry_event;
ALTER TABLE ff_worker_registry_event_new RENAME TO ff_worker_registry_event;
