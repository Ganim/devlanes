CREATE TABLE remote_hosts (
    name TEXT PRIMARY KEY CHECK(name != ''),
    transport TEXT NOT NULL,
    address TEXT NOT NULL,
    daemon_version TEXT,
    last_connected INTEGER
);
