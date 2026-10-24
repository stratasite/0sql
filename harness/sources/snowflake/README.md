# A throwaway key pair for the Snowflake tests

Three spellings of one RSA key, generated for the tests and registered
nowhere. They prove that the signer reads PKCS#8 and PKCS#1, refuses an
encrypted key by name, and derives the fingerprint Snowflake shows as
`RSA_PUBLIC_KEY_FP`:

```
W/K39Ak+DVbIXnQSGsa+qi9HMOPiwrK3vLEAYTyLuF0=
```

which is what this prints:

```
openssl rsa -in rsa_key.p8 -pubout -outform DER | openssl dgst -sha256 -binary | openssl base64 -A
```

The encrypted copy's passphrase is `x`. Never register this key with an
account.
