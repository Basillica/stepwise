#!/usr/bin/env bash
# Run the PostgreSQL-backed tests against a throwaway container.
#   ./scripts/test-postgres.sh                 # store contract + CRUD executor + HTTP-level suites
# Needs docker. To use your own server instead, skip this script and set
#   STEPWISE_TEST_POSTGRES_URL=postgres://user:pass@host:5432/db
# (tests create and drop their own `t_<random>` schemas; use a throwaway database).
set -euo pipefail
name=stepwise-pg-test
port=${PGPORT_TEST:-55432}
docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" -e POSTGRES_PASSWORD=pw -p "$port:5432" postgres:16 >/dev/null
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
echo -n "waiting for postgres"
for _ in $(seq 1 60); do
  if docker exec "$name" psql -U postgres -tc 'select 1' >/dev/null 2>&1; then break; fi
  echo -n .; sleep 1
done
echo
export STEPWISE_TEST_POSTGRES_URL="postgres://postgres:pw@127.0.0.1:$port/postgres"
cd "$(dirname "$0")/.."
cargo test -p stepwise-store                               # store: contract + CRUD executor, SQLite and this PostgreSQL
cargo test --test db --test effects -- --test-threads=4    # server: HTTP-level suites on the same backend
