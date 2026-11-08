//! The instance's VAPID identity (RFC 8292): the key pair a push service uses to recognise pushes
//! as coming from this hub.
//!
//! A browser subscribes against a public key (`applicationServerKey`), and the push service then
//! accepts pushes for that subscription only when they carry a token signed with the matching private
//! key. The pair is therefore made once per instance and kept: replacing it silently invalidates
//! every subscription.
//!
//! The installer generates the private key (P-256, PKCS#8 PEM) next to the instance's other keys and
//! hands the hub its path (`HUB_VAPID_KEY`), as it does the hub's client key: the hub has no secret to
//! encrypt it with, and a key file the operator backs up with the others outlives the database.

use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use serde::Serialize;
use time::OffsetDateTime;

/// How long a signed token stays valid. RFC 8292 caps it at a day; Apple refuses more than an hour.
const TOKEN_TTL_SECS: i64 = 60 * 60;

/// The instance's key pair, ready to sign.
pub struct VapidKeys {
    signing: SigningKey,
    /// The public key as a browser expects it: uncompressed point, base64url without padding.
    pub public_key: String,
}

/// Base64url without padding (RFC 4648 section 5), the only encoding VAPID and JWT use.
///
/// Written out rather than pulled in: a crate for thirty lines of well-specified encoding is one
/// more dependency to vet for no gain.
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = match chunk.len() {
            3 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8 | u32::from(chunk[2]),
            2 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8,
            _ => u32::from(chunk[0]) << 16,
        };
        let symbols = chunk.len() + 1;
        for i in 0..symbols {
            let index = (n >> (18 - 6 * i)) & 0x3f;
            out.push(char::from(ALPHABET[index as usize]));
        }
    }
    out
}

fn public_key_of(signing: &SigningKey) -> String {
    base64url(signing.verifying_key().to_sec1_point(false).as_bytes())
}

impl VapidKeys {
    /// The key pair from its PEM text (PKCS#8, as the installer writes it).
    pub fn from_pem(pem: &str) -> Result<Self, String> {
        let signing =
            SigningKey::from_pkcs8_pem(pem).map_err(|e| format!("not a P-256 key: {e}"))?;
        Ok(Self {
            public_key: public_key_of(&signing),
            signing,
        })
    }

    /// The key pair from the file `HUB_VAPID_KEY` names.
    pub fn read(path: &std::path::Path) -> Result<Self, String> {
        let pem = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::from_pem(&pem).map_err(|error| format!("{}: {error}", path.display()))
    }
}

#[derive(Serialize)]
struct Claims<'a> {
    aud: &'a str,
    exp: i64,
    sub: &'a str,
}

impl VapidKeys {
    /// The `Authorization` header for one push: a JWT (ES256) naming the push service's origin,
    /// followed by the public key it must verify against.
    ///
    /// `audience` is the origin of the subscription's endpoint (`https://fcm.googleapis.com`), and
    /// `subject` how the push service can reach whoever runs this instance (a `mailto:` or an
    /// `https:` URL).
    pub fn authorization(&self, audience: &str, subject: &str, now: OffsetDateTime) -> String {
        let header = base64url(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::to_vec(&Claims {
            aud: audience,
            exp: now.unix_timestamp() + TOKEN_TTL_SECS,
            sub: subject,
        })
        .unwrap_or_default();
        let signing_input = format!("{header}.{}", base64url(&claims));
        let signature: Signature = self.signing.sign(signing_input.as_bytes());
        format!(
            "vapid t={signing_input}.{}, k={}",
            base64url(&signature.to_bytes()),
            self.public_key
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;

    #[test]
    fn base64url_matches_the_rfc_vectors() {
        // RFC 4648 section 10, minus the padding, in the URL-safe alphabet.
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    /// A fixed test key (RFC 6979, appendix A.2.5), in PKCS#8 PEM.
    fn test_keys() -> VapidKeys {
        use p256::pkcs8::EncodePrivateKey;
        let scalar = [
            0xc9, 0xaf, 0xa9, 0xd8, 0x45, 0xba, 0x75, 0x16, 0x6b, 0x5c, 0x21, 0x57, 0x67, 0xb1,
            0xd6, 0x93, 0x4e, 0x50, 0xc3, 0xdb, 0x36, 0xe8, 0x9b, 0x12, 0x7b, 0x8a, 0x62, 0x2b,
            0x12, 0x0f, 0x67, 0x21,
        ];
        let signing = SigningKey::from_slice(&scalar).unwrap();
        let pem = signing.to_pkcs8_pem(Default::default()).unwrap();
        VapidKeys::from_pem(&pem).unwrap()
    }

    #[test]
    fn a_key_file_that_is_not_a_p256_key_is_refused() {
        assert!(VapidKeys::from_pem(
            "-----BEGIN PRIVATE KEY-----
AAAA
-----END PRIVATE KEY-----
"
        )
        .is_err());
        assert!(VapidKeys::from_pem("").is_err());
    }

    #[test]
    fn the_public_key_is_an_uncompressed_point() {
        let signing = test_keys().signing;
        // 65 bytes (0x04, x, y) encode to 87 characters without padding.
        let public = public_key_of(&signing);
        assert_eq!(public.len(), 87);
        assert!(public.starts_with('B'), "0x04 encodes as a leading B");
    }

    #[test]
    fn the_token_verifies_against_the_announced_key() {
        let keys = test_keys();
        let header = keys.authorization(
            "https://push.example",
            "mailto:admin@example.org",
            OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
        );
        let token = header
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split(", k=").next())
            .unwrap();
        let (signing_input, signature) = token.rsplit_once('.').unwrap();
        let signature_bytes = decode(signature);
        let signature = Signature::from_slice(&signature_bytes).unwrap();
        let verifying: &VerifyingKey = keys.signing.verifying_key();
        assert!(verifying
            .verify(signing_input.as_bytes(), &signature)
            .is_ok());

        let claims = decode(signing_input.split('.').nth(1).unwrap());
        let claims: serde_json::Value = serde_json::from_slice(&claims).unwrap();
        assert_eq!(claims["aud"], "https://push.example");
        assert_eq!(claims["sub"], "mailto:admin@example.org");
        assert_eq!(claims["exp"], 1_800_000_000 + TOKEN_TTL_SECS);
        assert!(header.ends_with(&format!("k={}", keys.public_key)));
    }

    /// Test-only decoder for the encoder above.
    fn decode(text: &str) -> Vec<u8> {
        const ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut bits = 0u32;
        let mut count = 0;
        let mut out = Vec::new();
        for c in text.chars() {
            bits = bits << 6 | ALPHABET.find(c).unwrap() as u32;
            count += 6;
            if count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
            }
        }
        out
    }
}
