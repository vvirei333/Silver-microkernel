# Vendored embedded-tls 0.19.0

Upstream: <https://github.com/drogue-iot/embedded-tls> (Apache-2.0).
`src/pki.rs` and one constant in `src/der_certificate.rs` are modified; tests,
examples and benches were removed.
The guest client uses the blocking API and the `rustpki`, `rsa`, `p384` and
`ed25519` features.

## Why

The stock `pki::CertVerifier` is single-root, assumes the server's chain ends
at a certificate signed directly by that root, and chooses the ECDSA verifier
from the *hash* in the child certificate's signature algorithm. The real web
breaks all three assumptions:

1. a browser needs several roots;
2. servers commonly include the trust anchor cross-signed by another root
   (ISRG Root X2 by X1, SSL.com by Comodo AAA), which the stock walk tries to
   verify through the wrong signature;
3. a P-384 root/intermediate can sign a certificate whose algorithm is
   `ecdsa-with-SHA256`, so a P-256 verifier is selected and key parsing fails;
4. only three SAN DNS names were retained, so a host that matches a wildcard
   late in a long SAN list (Wikipedia's `*.wikimedia.org`) was rejected.

## Patches

- `CertVerifier` stores an `Anchor` (single certificate or a slice of roots)
  and gained `new_multi(&[&[u8]])`. `verify_certificate` loops over the roots
  in one handshake and accepts the first that validates the chain and
  hostname.
- `verify_chain` finds the certificate in the server chain whose public key
  matches the anchor (the included, cross-signed copy) and verifies from the
  anchor down, skipping the cross signature. If the anchor is absent it starts
  at the root-most provided intermediate.
- `MAX_SAN_DNS_NAMES` is 32 instead of 3. Wikimedia certificates carry 40
  names and the matching wildcard can be at index 17; without this only a CN
  match (for example `en.wikipedia.org` against `*.wikipedia.org`) passed,
  while `thumb.wikimedia.org` failed even though the chain was valid.
- ECDSA chain verification chooses p-256 or p-384 from the CA key length
  (65 or 97 bytes) instead of from the child signature algorithm; both the
  SHA-256 and SHA-384 branches do this.

With these changes the host test client opens TLS 1.3 sessions to
`en.wikipedia.org`, `example.com`, `html.duckduckgo.com` (RSA certificate),
`www.bing.com` and `github.com` using six embedded roots.
