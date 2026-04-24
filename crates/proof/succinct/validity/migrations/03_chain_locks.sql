CREATE TABLE chain_locks (
    l1_chain_id BIGINT,
    l2_chain_id BIGINT,
    locked_at TIMESTAMP,
    PRIMARY KEY (l1_chain_id, l2_chain_id)
);