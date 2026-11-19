CREATE TABLE dbos_domain_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE dbos_invocations(
    dbos_id TEXT PRIMARY KEY,
    envelope TEXT NOT NULL,
    invocation_digest TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('admitted','submission_unknown','submitted','projected')),
    revision INTEGER NOT NULL,
    last_checked_ms INTEGER NOT NULL,
    runtime_status TEXT
);
CREATE INDEX dbos_invocation_bridge_scan ON dbos_invocations(state,last_checked_ms);
CREATE TABLE dbos_effect_receipts(
    effect_key TEXT PRIMARY KEY,
    dbos_id TEXT NOT NULL,
    invocation_digest TEXT NOT NULL,
    action_digest TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('uncertain','verified','not_applied')),
    receipt TEXT
);

CREATE TABLE dbos_effect_decisions(effect_key TEXT PRIMARY KEY,actor TEXT NOT NULL,decision TEXT NOT NULL,note TEXT NOT NULL,created_ms INTEGER NOT NULL);
