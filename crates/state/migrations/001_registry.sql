CREATE TABLE projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    identity BLOB NOT NULL UNIQUE,
    common_dir BLOB,
    root_path BLOB NOT NULL,
    primary_branch TEXT NOT NULL,
    last_start INTEGER
);
CREATE INDEX projects_name ON projects(name);
CREATE TABLE worktrees (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    identity TEXT NOT NULL,
    path BLOB NOT NULL,
    branch TEXT,
    present INTEGER NOT NULL CHECK(present IN (0, 1)),
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    UNIQUE(project_id, identity),
    UNIQUE(project_id, id)
);
CREATE TABLE lanes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    worktree_id INTEGER NOT NULL UNIQUE,
    lane_index INTEGER NOT NULL CHECK(lane_index >= 0),
    name TEXT NOT NULL,
    desired_state TEXT NOT NULL CHECK(desired_state IN ('running', 'stopped')),
    profile TEXT,
    created_at INTEGER NOT NULL,
    cleaned_at INTEGER,
    UNIQUE(project_id, id),
    FOREIGN KEY(project_id, worktree_id) REFERENCES worktrees(project_id, id)
);
CREATE UNIQUE INDEX lanes_active_index ON lanes(project_id, lane_index) WHERE cleaned_at IS NULL;
CREATE TABLE port_blocks (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    lane_id INTEGER UNIQUE REFERENCES lanes(id) ON DELETE CASCADE,
    start INTEGER NOT NULL CHECK(start BETWEEN 1 AND 65535),
    size INTEGER NOT NULL CHECK(size > 0 AND start + size <= 65536),
    FOREIGN KEY(project_id, lane_id) REFERENCES lanes(project_id, id)
);
CREATE UNIQUE INDEX port_blocks_shared ON port_blocks(project_id) WHERE lane_id IS NULL;
CREATE TRIGGER port_blocks_no_overlap_insert BEFORE INSERT ON port_blocks
WHEN EXISTS (SELECT 1 FROM port_blocks WHERE start < NEW.start + NEW.size AND NEW.start < start + size)
BEGIN SELECT RAISE(ABORT, 'overlapping port blocks'); END;
CREATE TRIGGER port_blocks_no_overlap_update BEFORE UPDATE ON port_blocks
WHEN EXISTS (SELECT 1 FROM port_blocks WHERE id != OLD.id AND start < NEW.start + NEW.size AND NEW.start < start + size)
BEGIN SELECT RAISE(ABORT, 'overlapping port blocks'); END;
CREATE TABLE port_offsets (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    unit TEXT NOT NULL,
    port_name TEXT NOT NULL,
    offset INTEGER NOT NULL CHECK(offset BETWEEN 0 AND 65534),
    reserved_by_expression INTEGER NOT NULL CHECK(reserved_by_expression IN (0, 1)),
    last_used INTEGER NOT NULL,
    PRIMARY KEY(project_id, unit, port_name),
    UNIQUE(project_id, offset),
    CHECK((reserved_by_expression = 1 AND unit = '') OR (reserved_by_expression = 0 AND unit != ''))
);
