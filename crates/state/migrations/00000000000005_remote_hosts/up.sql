CREATE TABLE remote_hosts (
    name TEXT PRIMARY KEY NOT NULL CHECK(name != ''),
    transport TEXT NOT NULL,
    address TEXT NOT NULL,
    daemon_version TEXT,
    last_connected BIGINT
);
CREATE INDEX remote_hosts_last_connected ON remote_hosts(last_connected);
