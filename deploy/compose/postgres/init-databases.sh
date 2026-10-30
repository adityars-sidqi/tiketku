#!/bin/sh
# Creates one database per service. Safe to run more than once: a database is
# only created when it does not exist yet.
# The postgres image runs this only on first start (empty data volume); to
# re-run it, reset the volume (see deploy/README.md).
set -eu

for db in auth_db catalog_db order_db payment_db notification_db; do
  echo "init-databases: ensuring database ${db}"
  # \gexec runs the generated CREATE DATABASE only when the SELECT returns a row.
  psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres <<EOSQL
SELECT 'CREATE DATABASE ${db}'
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = '${db}')\gexec
EOSQL
done

echo "init-databases: done"
