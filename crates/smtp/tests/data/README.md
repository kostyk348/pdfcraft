# TLS test receiver

A test-only key and certificates for the TLS tests (`crates/smtp/tests/tls.rs`). They protect
nothing and are never used by the app: the client is pointed at them with
`Server::extra_roots`, which adds a trust anchor for that one call.

| File | What |
|---|---|
| `ca.crt.der` | "PdfCraft Test SMTP Root": a P-256 test CA the tests add as a trust anchor |
| `receiver.crt.der` | `CN=localhost`, issued by that CA, SAN `DNS:localhost` and `IP:127.0.0.1`, `serverAuth` |
| `receiver.key.der` | the receiver's P-256 private key (PKCS#8, DER) |

Regenerate with (OpenSSL 3, from this directory):

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout ca.key -out ca.crt \
  -days 3650 -subj "/CN=PdfCraft Test SMTP Root/O=PdfCraft Tests" -set_serial 1 \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout receiver.key -out receiver.csr \
  -subj "/CN=localhost/O=PdfCraft Tests"
printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n" > receiver.ext
openssl x509 -req -in receiver.csr -CA ca.crt -CAkey ca.key -set_serial 4004 -days 3650 \
  -extfile receiver.ext -out receiver.crt
openssl x509 -in ca.crt -outform DER -out ca.crt.der
openssl x509 -in receiver.crt -outform DER -out receiver.crt.der
openssl pkcs8 -topk8 -nocrypt -in receiver.key -outform DER -out receiver.key.der
```

The plain files and the CA key are then deleted: the tests read only the three `.der` files above.
