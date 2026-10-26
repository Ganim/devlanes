-- Boxes connected with `remote connect` and not disconnected since, which a
-- restarted daemon connects again. Rows from earlier daemons start unwanted.
ALTER TABLE remote_hosts ADD COLUMN wanted INTEGER NOT NULL DEFAULT 0 CHECK(wanted IN (0, 1));
