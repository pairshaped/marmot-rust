-- func: find_item
SELECT id, name, note, enabled FROM item_names WHERE id = @id;

-- func: create_item
INSERT INTO items (name, note, enabled) VALUES (@name, @note, @enabled) RETURNING id;

-- func: update_item
-- columns: name, note
UPDATE items SET {{column}} = @value WHERE id = @id;
