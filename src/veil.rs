//! The Veil hybrid cryptosystem.

use std::fmt::{Debug, Formatter};
use std::io::{BufWriter, Read, Write};
use std::str::FromStr;
use std::{fmt, io, iter};

use rand::prelude::SliceRandom;
use rand::Rng;
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

pub use crate::mres::DecryptionError;
use crate::ristretto::{CanonicallyEncoded, Point, Scalar, G};
pub use crate::schnorr::{Signature, SignatureError, VerificationError};
use crate::schnorr::{Signer, Verifier};
use crate::{hkd, mres, pbenc};

/// A 512-bit secret from which multiple private keys can be derived.
#[derive(ZeroizeOnDrop)]
pub struct SecretKey {
    r: Vec<u8>,
}

impl SecretKey {
    /// Return a randomly generated secret key.
    #[must_use]
    pub fn new() -> SecretKey {
        SecretKey { r: rand::thread_rng().gen::<[u8; SECRET_KEY_LEN]>().to_vec() }
    }

    /// Encrypt the secret key with the given passphrase and `veil.pbenc` parameters.
    #[must_use]
    pub fn encrypt(&self, passphrase: &str, time: u32, space: u32) -> Vec<u8> {
        pbenc::encrypt(passphrase, time, space, &self.r)
    }

    /// Decrypt the secret key with the given passphrase.
    pub fn decrypt(passphrase: &str, ciphertext: &[u8]) -> Result<SecretKey, DecryptionError> {
        // Check the ciphertext length.
        if ciphertext.len() != SECRET_KEY_LEN + pbenc::OVERHEAD {
            return Err(DecryptionError::InvalidCiphertext);
        }

        // Decrypt the ciphertext and use the plaintext as the secret key.
        pbenc::decrypt(passphrase, ciphertext)
            .map(|r| SecretKey { r })
            .ok_or(DecryptionError::InvalidCiphertext)
    }

    /// The root private key.
    #[must_use]
    pub fn private_key(&self) -> PrivateKey {
        let d = hkd::root_key(&self.r);
        PrivateKey { d, pk: PublicKey { q: &d * &G } }
    }

    /// The root public key.
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.private_key().public_key()
    }
}

impl Default for SecretKey {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for SecretKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.public_key().fmt(f)
    }
}

const SECRET_KEY_LEN: usize = 64;

/// A derived private key, used to encrypt, decrypt, and sign messages.
#[derive(ZeroizeOnDrop)]
pub struct PrivateKey {
    d: Scalar,
    pk: PublicKey,
}

impl PrivateKey {
    /// Return the corresponding public key.
    #[must_use]
    pub const fn public_key(&self) -> PublicKey {
        self.pk
    }

    /// Encrypt the contents of the reader and write the ciphertext to the writer.
    ///
    /// Optionally add a number of fake recipients to disguise the number of true recipients and/or
    /// random padding to disguise the message length.
    pub fn encrypt(
        &self,
        reader: &mut impl Read,
        writer: &mut impl Write,
        recipients: &[PublicKey],
        fakes: usize,
        padding: u64,
    ) -> io::Result<u64> {
        // Add fakes.
        let mut q_rs = recipients
            .iter()
            .map(|pk| pk.q)
            .chain(iter::repeat_with(|| Point::random(&mut rand::thread_rng())).take(fakes))
            .collect::<Vec<Point>>();

        // Shuffle the recipients list.
        q_rs.shuffle(&mut rand::thread_rng());

        // Finally, encrypt.
        mres::encrypt(reader, &mut BufWriter::new(writer), &self.d, &self.pk.q, &q_rs, padding)
    }

    /// Decrypt the contents of the reader, if possible, and write the plaintext to the writer.
    ///
    /// If the ciphertext has been modified, was not sent by the sender, or was not encrypted for
    /// this private key, returns [DecryptionError::InvalidCiphertext].
    pub fn decrypt(
        &self,
        reader: &mut impl Read,
        writer: &mut impl Write,
        sender: &PublicKey,
    ) -> Result<u64, DecryptionError> {
        mres::decrypt(reader, &mut BufWriter::new(writer), &self.d, &self.pk.q, &sender.q)
    }

    /// Read the contents of the reader and return a digital signature.
    pub fn sign(&self, reader: &mut impl Read) -> io::Result<Signature> {
        let mut signer = Signer::new(io::sink());
        io::copy(reader, &mut signer)?;
        let (sig, _) = signer.sign(&self.d, &self.pk.q)?;
        Ok(sig)
    }

    /// Derive a private key with the given label sequence.
    #[must_use]
    pub fn derive<T>(&self, labels: &[T]) -> PrivateKey
    where
        T: AsRef<[u8]>,
    {
        let (d, q) = labels.iter().fold((self.d, self.pk.q), |(d, q), l| {
            let d = d + hkd::label_scalar(&q, l);
            (d, &d * &G)
        });
        PrivateKey { d, pk: PublicKey { q } }
    }
}

impl Eq for PrivateKey {}

impl PartialEq for PrivateKey {
    fn eq(&self, other: &Self) -> bool {
        self.pk == other.pk
    }
}

impl Debug for PrivateKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.pk.fmt(f)
    }
}

/// Error due to invalid public key format.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid public key")]
pub struct PublicKeyError;

/// A derived public key, used to verify messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Zeroize)]
pub struct PublicKey {
    q: Point,
}

impl PublicKey {
    /// Read the contents of the reader and return `Ok(())` iff the given signature was created by
    /// this public key of the exact contents. Otherwise, returns
    /// [VerificationError::InvalidSignature].
    pub fn verify(&self, reader: &mut impl Read, sig: &Signature) -> Result<(), VerificationError> {
        let mut verifier = Verifier::new();
        io::copy(reader, &mut verifier)?;

        verifier.verify(&self.q, sig)
    }

    /// Derive a public key with the given label sequence.
    #[must_use]
    pub fn derive<T>(&self, labels: &[T]) -> PublicKey
    where
        T: AsRef<[u8]>,
    {
        PublicKey { q: labels.iter().fold(self.q, |q, l| q + &hkd::label_scalar(&q, l) * &G) }
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", bs58::encode(&self.q.to_canonical_encoding()).into_string())
    }
}

impl FromStr for PublicKey {
    type Err = PublicKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        bs58::decode(s)
            .into_vec()
            .ok()
            .and_then(|b| Point::from_canonical_encoding(&b))
            .map(|q| PublicKey { q })
            .ok_or(PublicKeyError)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn hierarchical_key_derivation() {
        let sk = SecretKey::new();

        let abc = sk.private_key().derive(&["a", "b", "c"]).public_key();
        let abc_p = sk.public_key().derive(&["a", "b", "c"]);
        assert_eq!(abc, abc_p, "invalid hierarchical derivation");

        let abc = sk.private_key().derive(&["a", "b", "c"]);
        let cba = sk.private_key().derive(&["c", "b", "a"]);
        assert_ne!(abc, cba, "invalid hierarchical derivation");

        let abc = sk.private_key().derive(&["a"]).derive(&["b"]).derive(&["c"]).public_key();
        let abc_p = sk.private_key().derive(&["a", "b", "c"]).public_key();
        assert_eq!(abc, abc_p, "invalid hierarchical derivation");
    }

    #[test]
    fn public_key_encoding() {
        let base = PublicKey { q: G.basepoint() };
        assert_eq!(
            "GGumV86X6FZzHRo8bLvbW2LJ3PZ45EqRPWeogP8ufcm3",
            base.to_string(),
            "invalid encoded public key"
        );

        let decoded = "GGumV86X6FZzHRo8bLvbW2LJ3PZ45EqRPWeogP8ufcm3".parse::<PublicKey>();
        assert_eq!(Ok(base), decoded, "error parsing public key");

        assert_eq!(
            Err(PublicKeyError),
            "woot woot".parse::<PublicKey>(),
            "decoded invalid public key"
        );
    }

    #[test]
    fn round_trip() -> Result<(), DecryptionError> {
        let sk_a = SecretKey::new();
        let priv_a = sk_a.private_key();

        let sk_b = SecretKey::new();
        let priv_b = sk_b.private_key();

        let message = b"this is a thingy";
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        let ctx_len = priv_a.encrypt(&mut src, &mut dst, &[priv_b.public_key()], 20, 123)?;
        assert_eq!(dst.position(), ctx_len, "returned/observed ciphertext length mismatch");

        let mut src = Cursor::new(dst.into_inner());
        let mut dst = Cursor::new(Vec::new());

        let ptx_len = priv_b.decrypt(&mut src, &mut dst, &priv_a.public_key())?;
        assert_eq!(dst.position(), ptx_len, "returned/observed plaintext length mismatch");
        assert_eq!(message.to_vec(), dst.into_inner(), "incorrect plaintext");

        Ok(())
    }

    macro_rules! assert_failed {
        ($action: expr) => {
            match $action {
                Ok(_) => panic!("decrypted but shouldn't have"),
                Err(DecryptionError::InvalidCiphertext) => Ok(()),
                Err(e) => Err(e),
            }
        };
    }

    #[test]
    fn bad_sender_key() -> Result<(), DecryptionError> {
        let sk_a = SecretKey::new();
        let priv_a = sk_a.private_key();

        let sk_b = SecretKey::new();
        let priv_b = sk_b.private_key();

        let message = b"this is a thingy";
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        let ctx_len = priv_a.encrypt(&mut src, &mut dst, &[priv_b.public_key()], 20, 123)?;
        assert_eq!(dst.position(), ctx_len, "returned/observed ciphertext length mismatch");

        let mut src = Cursor::new(dst.into_inner());
        let mut dst = Cursor::new(Vec::new());

        assert_failed!(priv_b.decrypt(&mut src, &mut dst, &priv_b.public_key()))
    }

    #[test]
    fn bad_recipient() -> Result<(), DecryptionError> {
        let sk_a = SecretKey::new();
        let priv_a = sk_a.private_key();

        let sk_b = SecretKey::new();
        let priv_b = sk_b.private_key();

        let message = b"this is a thingy";
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        let ctx_len = priv_a.encrypt(&mut src, &mut dst, &[priv_a.public_key()], 20, 123)?;
        assert_eq!(dst.position(), ctx_len, "returned/observed ciphertext length mismatch");

        let mut src = Cursor::new(dst.into_inner());
        let mut dst = Cursor::new(Vec::new());

        assert_failed!(priv_b.decrypt(&mut src, &mut dst, &priv_a.public_key()))
    }

    #[test]
    fn bad_ciphertext() -> Result<(), DecryptionError> {
        let sk_a = SecretKey::new();
        let priv_a = sk_a.private_key();

        let sk_b = SecretKey::new();
        let priv_b = sk_b.private_key();

        let message = b"this is a thingy";
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        let ctx_len = priv_a.encrypt(&mut src, &mut dst, &[priv_b.public_key()], 20, 123)?;
        assert_eq!(dst.position(), ctx_len, "returned/observed ciphertext length mismatch");

        let mut ciphertext = dst.into_inner();
        ciphertext[200] ^= 1;

        let mut src = Cursor::new(ciphertext);
        let mut dst = Cursor::new(Vec::new());

        assert_failed!(priv_b.decrypt(&mut src, &mut dst, &priv_a.public_key()))
    }

    #[test]
    fn sign_and_verify() -> Result<(), VerificationError> {
        let sk = SecretKey::new();
        let message = b"this is a thingy";
        let mut src = Cursor::new(message);

        let sig = sk.private_key().sign(&mut src)?;

        let mut src = Cursor::new(message);
        sk.public_key().verify(&mut src, &sig)
    }
}
