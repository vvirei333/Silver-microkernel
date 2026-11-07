//! Bundle manifest policy: write scope, quota, integrity and authenticity.
//!
//! The manifest is a small text file next to the ELF (`app.elf` →
//! `app.manifest`). Directives:
//!
//! * `write=<absolute prefix>` — the only subtree the process may write.
//! * `quota=<blocks>:<inodes>` — hard limit for that subtree.
//! * `sha256=<64 hex>` — digest of the exact ELF bytes.
//! * `sig=<128 hex>` — Ed25519 signature over that digest.
//!
//! Parsing is deliberately forgiving for unknown lines (forward compatibility)
//! but fail-closed for policy mistakes: a quota without a write prefix is
//! dropped as a whole instead of silently granting an unbounded app.

use crate::{bundle_sig, sha256};

#[derive(Clone, Default)]
pub struct BundlePolicy {
    pub write_prefix: Option<alloc::string::String>,
    pub quota: Option<(u32, u32)>,
    pub hash: Option<[u8; 32]>,
    pub signature: Option<[u8; 64]>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VerifyError {
    /// A trusted key is provisioned but the manifest has no digest at all.
    MissingDigest,
    /// The ELF bytes do not match `sha256=`.
    HashMismatch,
    /// A trusted key is provisioned and the manifest carries no signature.
    MissingSignature,
    /// The signature does not verify over the digest.
    BadSignature,
    /// No trusted key is compiled in, so a claimed signature cannot be
    /// authenticated. Treating it as valid would be worse than refusing.
    UnauthenticatedSignature,
}

/// Parses manifest text. Unknown and malformed lines are ignored, except that
/// a quota without a usable write prefix invalidates both directives.
pub fn parse_manifest(text: &str) -> BundlePolicy {
    const SCOPE_MAX: usize = 96;
    let mut policy = BundlePolicy::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(scope) = line.strip_prefix("write=") {
            let scope = scope.trim();
            if scope.len() <= SCOPE_MAX
                && scope.starts_with('/')
                && !scope.contains("..")
                && !scope.ends_with('/')
            {
                policy.write_prefix = Some(alloc::string::String::from(scope));
            }
            continue;
        }
        if let Some(hash) = line.strip_prefix("sha256=") {
            if let Some(digest) = sha256::parse_hex(hash.trim()) {
                policy.hash = Some(digest);
            }
            continue;
        }
        if let Some(signature) = line.strip_prefix("sig=") {
            if let Some(bytes) = bundle_sig::parse_signature(signature.trim()) {
                policy.signature = Some(bytes);
            }
            continue;
        }
        if let Some(quota) = line.strip_prefix("quota=") {
            let mut parts = quota.trim().split(':');
            let (Some(blocks), Some(inodes), None) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(blocks), Ok(inodes)) = (blocks.parse::<u32>(), inodes.parse::<u32>()) else {
                continue;
            };
            if blocks > 0 && inodes > 0 {
                policy.quota = Some((blocks, inodes));
            }
        }
    }
    // A quota without a write prefix is meaningless and must not be silently
    // ignored: refuse the bundle policy rather than grant an unbounded app.
    if policy.quota.is_some() && policy.write_prefix.is_none() {
        policy.quota = None;
        policy.write_prefix = None;
    }
    policy
}

/// Applies the policy to the exact bytes that are about to become a task.
///
/// Order matters: the digest is checked before the signature, so a valid
/// signature over a different file can never authenticate this one.
pub fn verify(
    bundle: &[u8],
    policy: &BundlePolicy,
    trusted: Option<[u8; 32]>,
) -> Result<(), VerifyError> {
    match trusted {
        Some(key) => {
            let Some(expected) = policy.hash else {
                return Err(VerifyError::MissingDigest);
            };
            if sha256::digest(bundle) != expected {
                return Err(VerifyError::HashMismatch);
            }
            let Some(signature) = policy.signature else {
                return Err(VerifyError::MissingSignature);
            };
            if !bundle_sig::verify(&signature, &expected, &key) {
                return Err(VerifyError::BadSignature);
            }
            Ok(())
        }
        None => {
            if policy.signature.is_some() {
                return Err(VerifyError::UnauthenticatedSignature);
            }
            if let Some(expected) = policy.hash {
                if sha256::digest(bundle) != expected {
                    return Err(VerifyError::HashMismatch);
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];

    fn signing() -> SigningKey {
        SigningKey::from_bytes(&SEED)
    }

    fn hex(bytes: &[u8]) -> alloc::string::String {
        let mut text = alloc::string::String::new();
        for byte in bytes {
            use core::fmt::Write as _;
            let _ = write!(&mut text, "{byte:02x}");
        }
        text
    }

    fn signed_manifest(bundle: &[u8]) -> alloc::string::String {
        let digest = sha256::digest(bundle);
        let signature = signing().sign(&digest).to_bytes();
        alloc::format!(
            "write=/sfs/demo\nquota=8:5\nsha256={}\nsig={}\n",
            hex(&digest),
            hex(&signature)
        )
    }

    #[test]
    fn parses_scope_quota_hash_and_signature() {
        let bundle = b"elf";
        let policy = parse_manifest(&signed_manifest(bundle));
        assert_eq!(policy.write_prefix.as_deref(), Some("/sfs/demo"));
        assert_eq!(policy.quota, Some((8, 5)));
        assert_eq!(policy.hash, Some(sha256::digest(bundle)));
        assert!(policy.signature.is_some());
    }

    #[test]
    fn quota_requires_a_scope() {
        let policy = parse_manifest("quota=8:5\n");
        assert_eq!(policy.quota, None);
        assert_eq!(policy.write_prefix, None);
    }

    #[test]
    fn unsigned_bundle_is_accepted_without_a_key() {
        let bundle = b"elf";
        let policy = parse_manifest("sha256=");
        assert_eq!(verify(bundle, &policy, None), Ok(()));
        let hashed = parse_manifest(&alloc::format!(
            "sha256={}\n",
            hex(&sha256::digest(bundle))
        ));
        assert_eq!(verify(bundle, &hashed, None), Ok(()));
    }

    #[test]
    fn signed_bundle_verifies_only_with_the_right_key_and_bytes() {
        let bundle = b"elf bytes";
        let policy = parse_manifest(&signed_manifest(bundle));
        let key = signing().verifying_key().to_bytes();
        assert_eq!(verify(bundle, &policy, Some(key)), Ok(()));
        assert_eq!(
            verify(b"other bytes", &policy, Some(key)),
            Err(VerifyError::HashMismatch)
        );

        let mut wrong = key;
        wrong[0] ^= 1;
        assert_eq!(
            verify(bundle, &policy, Some(wrong)),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn keyed_build_requires_integrity_and_signature() {
        let key = signing().verifying_key().to_bytes();
        let unsigned = parse_manifest(&alloc::format!(
            "sha256={}\n",
            hex(&sha256::digest(b"elf"))
        ));
        assert_eq!(
            verify(b"elf", &unsigned, Some(key)),
            Err(VerifyError::MissingSignature)
        );
        assert_eq!(
            verify(b"elf", &BundlePolicy::default(), Some(key)),
            Err(VerifyError::MissingDigest)
        );
    }

    #[test]
    fn a_signature_without_a_key_is_refused() {
        let bundle = b"elf";
        let policy = parse_manifest(&signed_manifest(bundle));
        assert_eq!(
            verify(bundle, &policy, None),
            Err(VerifyError::UnauthenticatedSignature)
        );
    }
}
