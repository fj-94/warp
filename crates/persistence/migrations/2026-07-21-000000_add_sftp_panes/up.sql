CREATE TABLE sftp_panes (
    id INTEGER PRIMARY KEY NOT NULL REFERENCES pane_nodes(id) ON DELETE CASCADE,
    target TEXT,
    remote_path TEXT
);
