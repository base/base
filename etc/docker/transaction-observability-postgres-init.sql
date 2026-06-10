CREATE ROLE audit_archiver LOGIN PASSWORD 'audit_archiver';

GRANT CONNECT ON DATABASE transaction_observability TO audit_archiver;
