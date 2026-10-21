# Certificates for the test containers

A private CA and a server certificate it signed, the shape an enterprise
warehouse actually has, made with

    openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt -days 3650 \
      -subj "/CN=zsql test CA/O=zsql test" \
      -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
    openssl req -new -newkey rsa:2048 -nodes -keyout server.key -out server.csr \
      -subj "/CN=localhost/O=zsql test"
    openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
      -out server.crt -days 3650 -extfile <(printf 'basicConstraints=critical,CA:FALSE\n\
      keyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n\
      subjectAltName=DNS:localhost,IP:127.0.0.1\n')

The Postgres container serves `server.crt` and `server.key`, so that
`sslmode` can be proven rather than parsed: `require` accepts the
certificate as it is, `verify-full` refuses it as signed by a CA nobody
trusts, and `verify-full` with `ca_file: ca.crt` accepts it. A single
self-signed certificate will not do: a verifier rightly refuses a
`CA:TRUE` certificate presented as a server's (`CaUsedAsEndEntity`).

Both keys are committed on purpose and protect nothing: they are for a
throwaway container on loopback and nothing else.
