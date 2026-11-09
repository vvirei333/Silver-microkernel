use core::cmp::Ordering;
use der::asn1::{
    BitStringRef, GeneralizedTime, IntRef, ObjectIdentifier, SequenceOf, SetOf, UtcTime,
};
use der::{
    AnyRef, Choice, Decode, Enumerated, Header, Reader, Sequence, SliceReader, Tag, Tagged,
    ValueOrd,
};
use heapless::Vec;

// Real certificates list dozens of names: Wikimedia's wildcard has 40, and
// the host can match one late in the list (thumb.wikimedia.org matches
// *.wikimedia.org at index 17). Three entries only matched the CN.
pub const MAX_SAN_DNS_NAMES: usize = 32;
pub const HOSTNAME_MAXLEN: usize = 64;

const DNS_NAME_TAG: u8 = 0x82;
const COMMON_NAME_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.3");
const SUBJECT_ALT_NAME_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.17");
const BASIC_CONSTRAINTS_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.19");
const KEY_USAGE_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.15");

#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord, Sequence, ValueOrd)]
pub struct AlgorithmIdentifier<'a> {
    pub oid: ObjectIdentifier,
    pub parameters: Option<AnyRef<'a>>,
}

#[cfg(feature = "defmt")]
impl<'a> defmt::Format for AlgorithmIdentifier<'a> {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(fmt, "AlgorithmIdentifier:{}", &self.oid.as_bytes())
    }
}

pub const ECDSA_SHA256: AlgorithmIdentifier = AlgorithmIdentifier {
    oid: ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2"),
    parameters: None,
};
#[cfg(feature = "p384")]
pub const ECDSA_SHA384: AlgorithmIdentifier = AlgorithmIdentifier {
    oid: ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.3"),
    parameters: None,
};
#[cfg(feature = "ed25519")]
pub const ED25519: AlgorithmIdentifier = AlgorithmIdentifier {
    oid: ObjectIdentifier::new_unwrap("1.3.101.112"),
    parameters: None,
};
#[cfg(feature = "rsa")]
pub const RSA_PKCS1_SHA256: AlgorithmIdentifier = AlgorithmIdentifier {
    oid: ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11"),
    parameters: Some(AnyRef::NULL),
};
#[cfg(feature = "rsa")]
pub const RSA_PKCS1_SHA384: AlgorithmIdentifier = AlgorithmIdentifier {
    oid: ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.12"),
    parameters: Some(AnyRef::NULL),
};
#[cfg(feature = "rsa")]
pub const RSA_PKCS1_SHA512: AlgorithmIdentifier = AlgorithmIdentifier {
    oid: ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.13"),
    parameters: Some(AnyRef::NULL),
};

#[derive(Debug, Clone, PartialEq, Eq, Copy, Enumerated)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[asn1(type = "INTEGER")]
#[repr(u8)]
pub enum Version {
    /// Version 1 (default)
    V1 = 0,
    /// Version 2
    V2 = 1,
    /// Version 3
    V3 = 2,
}

impl ValueOrd for Version {
    fn value_cmp(&self, other: &Self) -> der::Result<Ordering> {
        (*self as u8).value_cmp(&(*other as u8))
    }
}

impl Default for Version {
    fn default() -> Self {
        Self::V1
    }
}

#[derive(Sequence, ValueOrd)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct DecodedCertificate<'a> {
    pub tbs_certificate: TbsCertificate<'a>,
    pub signature_algorithm: AlgorithmIdentifier<'a>,
    pub signature: BitStringRef<'a>,
}

#[derive(Debug, Sequence, ValueOrd)]
pub struct AttributeTypeAndValue<'a> {
    pub oid: ObjectIdentifier,
    pub value: AnyRef<'a>,
}

#[cfg(feature = "defmt")]
impl<'a> defmt::Format for AttributeTypeAndValue<'a> {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(
            fmt,
            "Attribute:{} Value:{}",
            &self.oid.as_bytes(),
            &self.value.value()
        )
    }
}

#[derive(Debug, Sequence, ValueOrd)]
pub struct ExtensionIdAndValue<'a> {
    pub extn_id: ObjectIdentifier,
    pub critical: Option<bool>,
    pub extn_value: AnyRef<'a>, // always type OCTET STRING
}

#[cfg(feature = "defmt")]
impl<'a> defmt::Format for ExtensionIdAndValue<'a> {
    fn format(&self, fmt: defmt::Formatter) {
        defmt::write!(
            fmt,
            "extnID:{} extnValue:{} critical:{}",
            &self.extn_id.as_bytes(),
            &self.extn_value.value(),
            &self.critical,
        )
    }
}

#[derive(Debug, Choice, ValueOrd)]
pub enum Time {
    #[asn1(type = "UTCTime")]
    UtcTime(UtcTime),

    #[asn1(type = "GeneralizedTime")]
    GeneralTime(GeneralizedTime),
}

#[cfg(feature = "defmt")]
impl defmt::Format for Time {
    fn format(&self, fmt: defmt::Formatter) {
        match self {
            Time::UtcTime(utc_time) => {
                defmt::write!(fmt, "UtcTime:{}", utc_time.to_unix_duration())
            }
            Time::GeneralTime(generalized_time) => {
                defmt::write!(fmt, "GeneralTime:{}", generalized_time.to_unix_duration())
            }
        }
    }
}

#[derive(Debug, Sequence, ValueOrd)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Validity {
    pub not_before: Time,
    pub not_after: Time,
}

#[derive(Debug, Sequence, ValueOrd)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct SubjectPublicKeyInfoRef<'a> {
    pub algorithm: AlgorithmIdentifier<'a>,
    pub public_key: BitStringRef<'a>,
}

#[derive(Debug, Sequence, ValueOrd)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct TbsCertificate<'a> {
    #[asn1(context_specific = "0", default = "Default::default")]
    pub version: Version,

    pub serial_number: IntRef<'a>,
    pub signature: AlgorithmIdentifier<'a>,
    pub issuer: SequenceOf<SetOf<AttributeTypeAndValue<'a>, 1>, 7>,

    pub validity: Validity,
    pub subject: SequenceOf<SetOf<AttributeTypeAndValue<'a>, 1>, 7>,
    pub subject_public_key_info: SubjectPublicKeyInfoRef<'a>,

    #[asn1(context_specific = "1", tag_mode = "IMPLICIT", optional = "true")]
    pub issuer_unique_id: Option<BitStringRef<'a>>,

    #[asn1(context_specific = "2", tag_mode = "IMPLICIT", optional = "true")]
    pub subject_unique_id: Option<BitStringRef<'a>>,

    #[asn1(context_specific = "3", tag_mode = "EXPLICIT", optional = "true")]
    pub extensions: Option<SequenceOf<ExtensionIdAndValue<'a>, 12>>,
}

/// Extract CommonName (CN) from the subject.
///
/// Parses the rdnSequence field of a TBS certificate, locates the
/// Common Name (OID 2.5.4.3), and returns it.
///
/// Returns `None` if no common name is present or does not fit into
/// `heapless::String<64>`.
pub fn extract_common_name<'a>(
    tbs: &TbsCertificate<'a>,
) -> Result<Option<heapless::String<64>>, der::Error> {
    let mut common_name = None;

    for elems in tbs.subject.iter() {
        let attrs = elems
            .get(0)
            .ok_or(der::ErrorKind::Value { tag: Tag::Set })?;

        if attrs.oid == COMMON_NAME_OID {
            let mut v: Vec<u8, HOSTNAME_MAXLEN> = Vec::new();
            v.extend_from_slice(attrs.value.value())
                .map_err(|_| der::ErrorKind::Value {
                    tag: Tag::Utf8String,
                })?;

            common_name = heapless::String::from_utf8(v).ok();
        }
    }
    Ok(common_name)
}

/// Extract DNS names from the Subject Alternative Name (SAN) extension.
///
/// Parses the extensions field of a TBS certificate, locates the SAN
/// extension (OID 2.5.29.17), and returns all GeneralName entries of
/// type dNSName (tag [2] IMPLICIT IA5String).
///
/// Returns an empty vector if no SAN extension is present.
pub fn extract_san_dns_names<'a>(
    tbs: &TbsCertificate<'a>,
) -> Result<heapless::Vec<heapless::String<HOSTNAME_MAXLEN>, MAX_SAN_DNS_NAMES>, der::Error> {
    let mut dns_names = heapless::Vec::new();

    let extensions_any = match &tbs.extensions {
        Some(ext) => ext,
        None => return Ok(dns_names),
    };

    for elems in extensions_any.iter() {
        if elems.extn_id != SUBJECT_ALT_NAME_OID {
            continue;
        }

        elems.extn_value.tag().assert_eq(Tag::OctetString)?;
        let mut san_reader = SliceReader::new(elems.extn_value.value())?;
        let san_header = Header::decode(&mut san_reader)?;
        san_header.tag().assert_eq(Tag::Sequence)?;
        let san_content = san_reader.read_slice(san_header.length())?;
        let mut gn_reader = SliceReader::new(san_content)?;
        while !gn_reader.is_finished() {
            let tag_byte = gn_reader.peek_byte().ok_or(der::ErrorKind::Incomplete {
                expected_len: 1u8.into(),
                actual_len: 0u8.into(),
            })?;

            let gn_header = Header::decode(&mut gn_reader)?;
            let gn_value = gn_reader.read_slice(gn_header.length())?;

            if tag_byte == DNS_NAME_TAG {
                // dNSName [2] IMPLICIT IA5String
                let gn_value_str =
                    core::str::from_utf8(gn_value).map_err(|_| der::ErrorKind::Value {
                        tag: Tag::Ia5String,
                    })?;

                let dns_name = heapless::String::try_from(gn_value_str).map_err(|_| {
                    der::ErrorKind::Value {
                        tag: Tag::Ia5String,
                    }
                })?;

                let _ = dns_names.push(dns_name);
                if dns_names.is_full() {
                    break;
                }
            }
        }
    }
    Ok(dns_names)
}

/// Minimal DER TLV reader for extension values. Extension bodies are tiny, so
/// the short-form length is the only form that can occur; rejecting the long
/// form keeps the arithmetic trivially bounded.
fn der_tlv(input: &[u8]) -> Option<(u8, &[u8], usize)> {
    let tag = *input.first()?;
    let length = *input.get(1)? as usize;
    if length & 0x80 != 0 {
        return None;
    }
    let end = 2usize.checked_add(length)?;
    if input.len() < end {
        return None;
    }
    Some((tag, &input[2..end], end))
}

/// Parses the DER content of a BasicConstraints extension. Split from the
/// extension walk so it can be unit-tested without constructing a certificate.
fn parse_basic_constraints(body: &[u8]) -> Option<(bool, Option<u64>)> {
    let mut body = body;
    let mut ca = false;
    if matches!(body.first(), Some(0x01)) {
        let (tag, value, used) = der_tlv(body)?;
        if tag != 0x01 || value.len() != 1 {
            return None;
        }
        ca = value[0] != 0;
        body = &body[used..];
    }
    let mut path_len = None;
    if matches!(body.first(), Some(0x02)) {
        let (tag, value, used) = der_tlv(body)?;
        if tag != 0x02 || value.is_empty() || value.len() > 4 {
            return None;
        }
        let mut number = 0_u64;
        for byte in value {
            number = (number << 8) | u64::from(*byte);
        }
        path_len = Some(number);
        body = &body[used..];
    }
    if !body.is_empty() {
        return None;
    }
    Some((ca, path_len))
}

/// Parses `BasicConstraints ::= SEQUENCE { cA BOOLEAN DEFAULT FALSE,
/// pathLenConstraint INTEGER OPTIONAL }`. `None` means the extension is absent
/// (a v1/v2 certificate or an end entity that omits it).
pub fn extract_basic_constraints(
    tbs: &TbsCertificate<'_>,
) -> Result<Option<(bool, Option<u64>)>, der::Error> {
    let Some(extensions) = &tbs.extensions else {
        return Ok(None);
    };
    for extension in extensions.iter() {
        if extension.extn_id != BASIC_CONSTRAINTS_OID {
            continue;
        }
        extension.extn_value.tag().assert_eq(Tag::OctetString)?;
        let Some((tag, body, _)) = der_tlv(extension.extn_value.value()) else {
            return Err(der::ErrorKind::Value { tag: Tag::Sequence }.into());
        };
        if tag != 0x30 {
            return Err(der::ErrorKind::Value { tag: Tag::Sequence }.into());
        }
        return parse_basic_constraints(body)
            .map(Some)
            .ok_or_else(|| der::ErrorKind::Value { tag: Tag::Sequence }.into());
    }
    Ok(None)
}

/// Parses the BIT STRING content of a KeyUsage extension (first byte is the
/// count of unused trailing bits). Split out for testing.
fn parse_key_usage(body: &[u8]) -> Option<bool> {
    if body.is_empty() {
        return None;
    }
    let unused = body[0] as usize;
    let bits = &body[1..];
    if unused > 7 || bits.is_empty() {
        return Some(false);
    }
    // Bit 5 is the third-most-significant bit of the first content byte. When
    // the whole bit string is shorter than six bits it cannot name
    // keyCertSign at all.
    if bits.len() == 1 && unused > 2 {
        return Some(false);
    }
    Some(bits[0] & 0x04 != 0)
}

/// Parses `KeyUsage ::= BIT STRING` and returns whether `keyCertSign` (bit 5)
/// is asserted. `None` means the extension is absent, which RFC 5280 permits
/// for CA certificates that predate the profile.
pub fn extract_key_cert_sign(tbs: &TbsCertificate<'_>) -> Result<Option<bool>, der::Error> {
    let Some(extensions) = &tbs.extensions else {
        return Ok(None);
    };
    for extension in extensions.iter() {
        if extension.extn_id != KEY_USAGE_OID {
            continue;
        }
        extension.extn_value.tag().assert_eq(Tag::OctetString)?;
        let Some((tag, body, _)) = der_tlv(extension.extn_value.value()) else {
            return Err(der::ErrorKind::Value { tag: Tag::BitString }.into());
        };
        if tag != 0x03 {
            return Err(der::ErrorKind::Value { tag: Tag::BitString }.into());
        }
        return parse_key_usage(body)
            .map(Some)
            .ok_or_else(|| der::ErrorKind::Value { tag: Tag::BitString }.into());
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{parse_basic_constraints, parse_key_usage};

    #[test]
    fn basic_constraints_requires_ca() {
        // SEQUENCE {} (CA defaults to FALSE)
        assert_eq!(parse_basic_constraints(&[]), Some((false, None)));
        // SEQUENCE { BOOLEAN FALSE }
        assert_eq!(
            parse_basic_constraints(&[0x01, 0x01, 0x00]),
            Some((false, None))
        );
        // SEQUENCE { BOOLEAN TRUE }
        assert_eq!(
            parse_basic_constraints(&[0x01, 0x01, 0xFF]),
            Some((true, None))
        );
    }

    #[test]
    fn basic_constraints_reads_path_length() {
        // SEQUENCE { BOOLEAN TRUE, INTEGER 0 }
        assert_eq!(
            parse_basic_constraints(&[0x01, 0x01, 0xFF, 0x02, 0x01, 0x00]),
            Some((true, Some(0)))
        );
        // SEQUENCE { BOOLEAN TRUE, INTEGER 3 }
        assert_eq!(
            parse_basic_constraints(&[0x01, 0x01, 0xFF, 0x02, 0x01, 0x03]),
            Some((true, Some(3)))
        );
    }

    #[test]
    fn basic_constraints_rejects_trailing_data() {
        assert_eq!(
            parse_basic_constraints(&[0x01, 0x01, 0xFF, 0x05, 0x00]),
            None
        );
    }

    #[test]
    fn key_usage_needs_key_cert_sign() {
        // unused=1, bits=0b0000_010x: keyCertSign (bit 5) set.
        assert_eq!(parse_key_usage(&[0x01, 0x04]), Some(true));
        // digitalSignature only.
        assert_eq!(parse_key_usage(&[0x07, 0x80]), Some(false));
        // KeyUsage with no bits to spare for bit 5.
        assert_eq!(parse_key_usage(&[0x07, 0x80]), Some(false));
        assert_eq!(parse_key_usage(&[]), None);
    }
}
