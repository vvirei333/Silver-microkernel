//! Ed25519 verification for bundle manifests.
//!
//! A manifest may carry `sig=<128 hex>`, the Ed25519 signature over the
//! bundle's 32-byte SHA-256 digest. Integrity (`sha256=`) is always checked
//! first; the signature then proves that the digest was produced by the
//! holder of the trusted private key.
//!
//! The trusted public key is compiled in from `SILVER_BUNDLE_PUBKEY`
//! (64 hex characters), which `build.rs` reads from `keys/bundle_ed25519.pub`
//! or the environment. With no key provisioned the kernel stays in
//! integrity-only mode and a manifest that claims a signature is rejected:
//! it cannot be authenticated, so trusting it would be worse than refusing it.

use ed25519_dalek::{Signature, VerifyingKey};

/// The compile-time trusted public key, if this build was provisioned with one.
pub fn trusted_key() -> Option<[u8; 32]> {
    let text = option_env!("SILVER_BUNDLE_PUBKEY")?;
    parse_pubkey(text)
}

/// True when this kernel enforces signatures for bundles that carry one.
/// Callers use it to describe policy at boot; the load path itself uses
/// [`verify`] and [`trusted_key`] directly.
pub fn signatures_enabled() -> bool {
    trusted_key().is_some()
}

/// Parses a 64-character hex Ed25519 public key.
pub fn parse_pubkey(text: &str) -> Option<[u8; 32]> {
    parse_hex::<32>(text)
}

/// Parses a 128-character hex Ed25519 signature.
pub fn parse_signature(text: &str) -> Option<[u8; 64]> {
    parse_hex::<64>(text)
}

/// Verifies `signature` over `digest` with `public_key`. Malformed keys,
/// signatures and non-canonical points are rejected rather than repaired.
pub fn verify(signature: &[u8; 64], digest: &[u8; 32], public_key: &[u8; 32]) -> bool {
    verify_message(signature, digest, public_key)
}

fn verify_message(signature: &[u8; 64], message: &[u8], public_key: &[u8; 32]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    let signature = Signature::from_bytes(signature);
    key.verify_strict(message, &signature).is_ok()
}

fn parse_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes = text.as_bytes();
    if bytes.len() != N * 2 {
        return None;
    }
    let mut output = [0_u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        let high = nibble(bytes[index * 2])?;
        let low = nibble(bytes[index * 2 + 1])?;
        *byte = (high << 4) | low;
    }
    Some(output)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8032, section 7.1, TEST 1 (empty message).
    const RFC_KEY: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const RFC_SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const RFC_SIG: &str = concat!(
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155",
        "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
    );

    #[test]
    fn rfc8032_vector_verifies() {
        let key = parse_pubkey(RFC_KEY).unwrap();
        let signature = parse_signature(RFC_SIG).unwrap();
        // The published vector signs the empty message byte string.
        assert!(verify_message(&signature, b"", &key));
    }

    #[test]
    fn digest_signature_round_trip() {
        use ed25519_dalek::{Signer, SigningKey};

        let seed = parse_pubkey(RFC_SEED).unwrap();
        let key = parse_pubkey(RFC_KEY).unwrap();
        let signing = SigningKey::from_bytes(&seed);
        assert_eq!(signing.verifying_key().to_bytes(), key);

        let digest = crate::sha256::digest(b"werewolf");
        let signature = signing.sign(&digest).to_bytes();
        assert!(verify(&signature, &digest, &key));

        let mut modified = digest;
        modified[0] ^= 1;
        assert!(!verify(&signature, &modified, &key));
    }

    #[test]
    fn rejects_modified_signature_and_key() {
        let key = parse_pubkey(RFC_KEY).unwrap();
        let mut signature = parse_signature(RFC_SIG).unwrap();
        assert!(verify_message(&signature, b"", &key));
        signature[0] ^= 1;
        assert!(!verify_message(&signature, b"", &key));

        let mut other_key = key;
        other_key[0] ^= 1;
        let signature = parse_signature(RFC_SIG).unwrap();
        assert!(!verify_message(&signature, b"", &other_key));
    }

    #[test]
    fn hex_parse_is_strict() {
        assert!(parse_pubkey("abc").is_none());
        assert!(parse_pubkey(&"z".repeat(64)).is_none());
        assert!(parse_signature(&"0".repeat(126)).is_none());
        assert!(parse_pubkey(&"A".repeat(64)).is_some());
    }
}
