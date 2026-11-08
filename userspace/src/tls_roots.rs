//! Trust anchors for the guest TLS client.
//!
//! The embedded-tls verifier accepts a list of roots and picks the one that
//! validates the server chain. These six cover the current search engine,
//! Wikipedia, example.com, Microsoft and the common Sectigo/USERTrust path;
//! more roots can be appended without touching the handshake code.
//!
//! The DER files are the public root certificates as shipped by the
//! certificate authorities (see each file's subject).

pub static ROOTS: [&[u8]; 6] = [
    include_bytes!("tls_roots/ISRG_Root_X2.der"),
    include_bytes!("tls_roots/ISRG_Root_X1.der"),
    include_bytes!("tls_roots/DigiCert_Global_Root_G2.der"),
    include_bytes!("tls_roots/DigiCert_Global_Root_G3.der"),
    include_bytes!("tls_roots/USERTrust_ECC_Certification_Authority.der"),
    include_bytes!("tls_roots/SSL.com_TLS_ECC_Root_CA_2022.der"),
];

/// No RTC exists yet (`date` is still a TODO), so certificate validity is
/// checked against a fixed build-time date. Replace with the RTC once it
/// exists; TLS will otherwise stop accepting newly issued certificates.
pub const FIXED_NOW: u64 = 1_794_137_600; // 2026-11-08T00:00:00Z
