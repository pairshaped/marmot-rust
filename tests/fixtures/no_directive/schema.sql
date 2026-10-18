CREATE TABLE items (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    note TEXT,
    enabled INTEGER NOT NULL CONSTRAINT boolean CHECK (enabled IN (0, 1))
) STRICT;
CREATE VIEW item_names AS SELECT id, name, note, enabled FROM items;
