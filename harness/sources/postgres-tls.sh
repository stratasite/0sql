#!/bin/sh
# Runs once, on the container's first start, from docker-entrypoint-initdb.d:
# installs the test pair where the server can own it and turns TLS on. A
# mounted key is owned by the host's user, which the server refuses, so
# the pair is copied rather than mounted in place.
set -e
cp /tls/server.crt /tls/server.key "$PGDATA/"
chmod 600 "$PGDATA/server.key"
cat >> "$PGDATA/postgresql.conf" <<EOF
ssl = on
ssl_cert_file = 'server.crt'
ssl_key_file = 'server.key'
EOF
