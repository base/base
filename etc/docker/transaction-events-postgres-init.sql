CREATE ROLE audit_archiver LOGIN PASSWORD 'audit_archiver';

GRANT CONNECT ON DATABASE transaction_events TO audit_archiver;
