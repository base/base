#!/bin/bash
set -e

echo "Running database migrations..."

for file in /migrations/*.sql; do
    if [ -f "$file" ]; then
        echo "Applying migration: $(basename "$file")"
        psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" < "$file"
    fi
done

echo "Database initialization completed successfully"