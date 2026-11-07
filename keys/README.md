# Bundle signing keys

The kernel verifies `sig=` in an app manifest with the Ed25519 public key that
was compiled into it. Provisioning is intentionally explicit:

* `keys/bundle_ed25519.pub` — 64 hex characters, read by `build.rs`.
* or `SILVER_BUNDLE_PUBKEY=<64 hex>` in the build environment.

With a key present, every ELF loaded through `SYS_SPAWN_PATH` (the `run`
command) must have a manifest containing both `sha256=` and a valid `sig=`;
without a key the kernel stays in integrity-only mode and refuses manifests
that claim a signature it cannot check.

Generate a development key and sign a bundle:

```sh
scripts/sign_bundle.py keygen /tmp/werewolf-dev
cp /tmp/werewolf-dev.pub keys/bundle_ed25519.pub
scripts/sign_bundle.py sign /tmp/werewolf-dev.key app.elf app.manifest
```

Never commit a private key (`.key`). `keys/*.key` is ignored by
`.gitignore`; the public key is safe to commit.
