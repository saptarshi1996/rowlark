#!/usr/bin/env bash
# Makes throwaway TLS certificates for the Postgres TLS test server
# (docker-compose service "postgres16tls"). Dev and CI only: the keys are
# regenerated on demand, never committed (.test-certs/ is in .gitignore).
#
#   scripts/gen-test-certs.sh        # writes into ./.test-certs
#
# Produces:
#   ca.crt / ca.key         a test certificate authority
#   server.crt / server.key certificate for the host name "localhost" ONLY
#                           (so tests can prove that verify-full rejects 127.0.0.1)
#   client.crt / client.key certificate whose name is "rowlark_cert", the
#                           database user that must log in with a certificate
set -euo pipefail

out="$(cd "$(dirname "$0")/.." && pwd)/.test-certs"
rm -rf "$out"
mkdir -p "$out"
cd "$out"

# A test certificate authority.
openssl genrsa -out ca.key 2048 2>/dev/null
openssl req -x509 -new -key ca.key -sha256 -days 365 \
  -subj "/CN=Rowlark Test CA" -out ca.crt

# Signs a request with the CA. $1 = name, $2 = extensions text.
sign() {
  printf '%s\n' "$2" > "$1.ext"
  openssl req -new -key "$1.key" -subj "/CN=$3" -out "$1.csr"
  openssl x509 -req -in "$1.csr" -CA ca.crt -CAkey ca.key -CAcreateserial \
    -out "$1.crt" -days 365 -sha256 -extfile "$1.ext" 2>/dev/null
}

openssl genrsa -out server.key 2048 2>/dev/null
sign server "subjectAltName=DNS:localhost
basicConstraints=CA:FALSE
extendedKeyUsage=serverAuth" localhost

openssl genrsa -out client.key 2048 2>/dev/null
sign client "basicConstraints=CA:FALSE
extendedKeyUsage=clientAuth" rowlark_cert

rm -f ./*.csr ./*.ext ./*.srl
chmod 644 ./*.crt ./*.key   # read by the Docker container, which runs as another user
echo "Test certificates written to $out"
