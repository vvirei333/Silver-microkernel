#!/usr/bin/env python3
"""Bundle signing helper for WerewolfOS.

The kernel checks `sha256=` for integrity and, when built with a trusted
public key, `sig=` (Ed25519 over the 32-byte digest) for authenticity.

Subcommands:
    keygen <prefix>             write <prefix>.key (secret seed) and <prefix>.pub
    sign <keyfile> <elf> <manifest>
    verify <pubkey> <elf> <manifest>

`keygen` uses the host's `cryptography` package; the kernel never sees the
private seed. Provision a build by copying the public key to
`keys/bundle_ed25519.pub` or exporting `SILVER_BUNDLE_PUBKEY=<64 hex>`.
"""
import hashlib
import os
import stat
import sys

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey

MAX_MANIFEST = 512


def load_seed(path):
    text = open(path, "r", encoding="ascii").read().strip()
    seed = bytes.fromhex(text)
    if len(seed) != 32:
        raise SystemExit(f"{path}: expected a 32-byte hex seed")
    return seed


def public_bytes(private):
    return private.public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw
    )


def keygen(prefix):
    private = Ed25519PrivateKey.generate()
    seed = private.private_bytes(
        serialization.Encoding.Raw,
        serialization.PrivateFormat.Raw,
        serialization.NoEncryption(),
    )
    key_path = prefix + ".key"
    pub_path = prefix + ".pub"
    with open(key_path, "w", encoding="ascii") as handle:
        handle.write(seed.hex() + "\n")
    os.chmod(key_path, stat.S_IRUSR | stat.S_IWUSR)
    with open(pub_path, "w", encoding="ascii") as handle:
        handle.write(public_bytes(private).hex() + "\n")
    print(f"wrote {key_path} (secret) and {pub_path}")
    print(f"provision with: export SILVER_BUNDLE_PUBKEY={public_bytes(private).hex()}")


def rewrite_manifest(path, digest, signature):
    lines = []
    try:
        with open(path, "r", encoding="utf-8") as handle:
            for line in handle.read().splitlines():
                stripped = line.strip()
                if stripped.startswith("sha256=") or stripped.startswith("sig="):
                    continue
                lines.append(line)
    except FileNotFoundError:
        pass
    lines.append(f"sha256={digest.hex()}")
    lines.append(f"sig={signature.hex()}")
    text = "\n".join(lines) + "\n"
    if len(text) > MAX_MANIFEST:
        raise SystemExit(
            f"manifest would be {len(text)} bytes; the kernel limit is {MAX_MANIFEST}"
        )
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)
    print(f"signed {path}: sha256={digest.hex()} sig={signature.hex()}")


def sign(keyfile, elf, manifest):
    private = Ed25519PrivateKey.from_private_bytes(load_seed(keyfile))
    digest = hashlib.sha256(open(elf, "rb").read()).digest()
    rewrite_manifest(manifest, digest, private.sign(digest))


def verify(pubfile, elf, manifest):
    text = open(pubfile, "r", encoding="ascii").read().strip()
    public = Ed25519PublicKey.from_public_bytes(bytes.fromhex(text))
    digest = hashlib.sha256(open(elf, "rb").read()).digest()
    entries = {}
    for line in open(manifest, "r", encoding="utf-8"):
        line = line.strip()
        if "=" in line and not line.startswith("#"):
            key, value = line.split("=", 1)
            entries[key] = value
    if entries.get("sha256") != digest.hex():
        raise SystemExit("manifest sha256 does not match the ELF")
    try:
        public.verify(bytes.fromhex(entries["sig"]), digest)
    except (KeyError, ValueError) as error:
        raise SystemExit(f"missing or malformed signature: {error}")
    except Exception:
        raise SystemExit("signature does not verify")
    print(f"OK: {manifest}")


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    command = sys.argv[1]
    if command == "keygen" and len(sys.argv) == 3:
        keygen(sys.argv[2])
    elif command == "sign" and len(sys.argv) == 5:
        sign(*sys.argv[2:5])
    elif command == "verify" and len(sys.argv) == 5:
        verify(*sys.argv[2:5])
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
