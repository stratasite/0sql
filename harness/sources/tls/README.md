# A certificate for the test containers

`server.crt` and `server.key` are a self-signed pair for `localhost`,
valid for ten years, made with

    openssl req -x509 -newkey rsa:2048 -nodes -keyout server.key -out server.crt \
      -days 3650 -subj "/CN=localhost/O=zsql test" \
      -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"

They let the Postgres container offer TLS, so that `sslmode: require`,
`verify-full` and `ca_file` can be proven rather than parsed: `require`
accepts the pair as it is, `verify-full` refuses it as signed by nobody,
and `verify-full` with `ca_file: server.crt` accepts it. The key is
committed on purpose and protects nothing: it is for a throwaway
container on loopback and nothing else.
