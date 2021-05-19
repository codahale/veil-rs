use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use strobe_rs::{SecParam, Strobe};

use crate::util::{StrobeExt, MAC_LEN, POINT_LEN};

/// The number of bytes encapsulation adds to a plaintext.
pub const OVERHEAD: usize = POINT_LEN + MAC_LEN;

/// Given a sender's key pair, an ephemeral key pair, and the recipient's public key, encrypt the
/// given plaintext.
pub fn encapsulate(
    d_s: &Scalar,
    q_s: &RistrettoPoint,
    d_e: &Scalar,
    q_e: &RistrettoPoint,
    q_r: &RistrettoPoint,
    plaintext: &[u8],
) -> Vec<u8> {
    // Allocate a buffer for output and fill it with plaintext.
    let mut out = vec![0u8; POINT_LEN + plaintext.len() + MAC_LEN];
    out[..POINT_LEN].copy_from_slice(q_e.compress().as_bytes());
    out[POINT_LEN..POINT_LEN + plaintext.len()].copy_from_slice(plaintext);

    // Initialize the protocol.
    let mut akem = Strobe::new(b"veil.akem", SecParam::B128);
    akem.meta_ad_u32(MAC_LEN as u32);

    // Include the sender and receiver as associated data.
    akem.ad_point(q_s);
    akem.ad_point(q_r);

    // Calculate the static Diffie-Hellman shared secret and key the protocol with it.
    akem.key_point(d_s * q_r);

    // Encrypt the ephemeral public key.
    akem.send_enc(&mut out[..POINT_LEN], false);

    // Calculate the ephemeral Diffie-Hellman shared secret and key the protocol with it.
    akem.key_point(d_e * q_r);

    // Encrypt the plaintext.
    akem.send_enc(&mut out[POINT_LEN..POINT_LEN + plaintext.len()], false);

    // Calculate a MAC of the entire operation transcript.
    akem.send_mac(&mut out[POINT_LEN + plaintext.len()..], false);

    // Return the encrypted ephemeral public key, the ciphertext, and the MAC.
    out
}

/// Given a recipient's key pair and sender's public key, recover the ephemeral public key and
/// plaintext from the given ciphertext.
pub fn decapsulate(
    d_r: &Scalar,
    q_r: &RistrettoPoint,
    q_s: &RistrettoPoint,
    ciphertext: &[u8],
) -> Option<(RistrettoPoint, Vec<u8>)> {
    // Ensure the ciphertext has a point and MAC, at least.
    if ciphertext.len() < POINT_LEN + MAC_LEN {
        return None;
    }

    // Copy the input for modification.
    let mut out = Vec::from(ciphertext);

    // Initialize the protocol.
    let mut akem = Strobe::new(b"veil.akem", SecParam::B128);
    akem.meta_ad_u32(MAC_LEN as u32);

    // Include the sender and receiver as associated data.
    akem.ad_point(q_s);
    akem.ad_point(q_r);

    // Calculate the static Diffie-Hellman shared secret and key the protocol with it.
    akem.key_point(d_r * q_s);

    // Decrypt the ephemeral public key.
    akem.recv_enc(&mut out[..POINT_LEN], false);

    // Decode the ephemeral public key.
    let q_e = CompressedRistretto::from_slice(&out[..POINT_LEN]).decompress()?;

    // Calculate the ephemeral Diffie-Hellman shared secret and key the protocol with it.
    akem.key_point(d_r * q_e);

    // Decrypt the plaintext.
    akem.recv_enc(&mut out[POINT_LEN..ciphertext.len() - MAC_LEN], false);

    // Verify the MAC.
    akem.recv_mac(&mut out[ciphertext.len() - MAC_LEN..]).ok()?;

    // Return the ephemeral public key and the plaintext.
    Some((q_e, out[POINT_LEN..ciphertext.len() - MAC_LEN].to_vec()))
}

#[cfg(test)]
mod tests {
    use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
    use curve25519_dalek::ristretto::RistrettoPoint;
    use curve25519_dalek::scalar::Scalar;

    use crate::util;

    use super::*;

    #[test]
    fn round_trip() {
        let (d_s, q_s, d_e, q_e, d_r, q_r) = setup();
        let ciphertext = encapsulate(&d_s, &q_s, &d_e, &q_e, &q_r, b"this is an example");
        let (pk, plaintext) = decapsulate(&d_r, &q_r, &q_s, &ciphertext).expect("decapsulate");

        assert_eq!(q_e, pk);
        assert_eq!(b"this is an example".to_vec(), plaintext);
    }

    #[test]
    fn bad_ephemeral_public_key() {
        let (d_s, q_s, d_e, q_e, d_r, q_r) = setup();
        let mut ciphertext = encapsulate(&d_s, &q_s, &d_e, &q_e, &q_r, b"this is an example");
        ciphertext[0] ^= 1;

        assert_eq!(None, decapsulate(&d_r, &q_r, &q_s, &ciphertext));
    }

    #[test]
    fn bad_ciphertext() {
        let (d_s, q_s, d_e, q_e, d_r, q_r) = setup();
        let mut ciphertext = encapsulate(&d_s, &q_s, &d_e, &q_e, &q_r, b"this is an example");
        ciphertext[36] ^= 1;

        assert_eq!(None, decapsulate(&d_r, &q_r, &q_s, &ciphertext));
    }

    #[test]
    fn bad_mac() {
        let (d_s, q_s, d_e, q_e, d_r, q_r) = setup();
        let mut ciphertext = encapsulate(&d_s, &q_s, &d_e, &q_e, &q_r, b"this is an example");
        ciphertext[64] ^= 1;

        assert_eq!(None, decapsulate(&d_r, &q_r, &q_s, &ciphertext));
    }

    fn setup() -> (Scalar, RistrettoPoint, Scalar, RistrettoPoint, Scalar, RistrettoPoint) {
        let d_s = util::rand_scalar();
        let q_s = RISTRETTO_BASEPOINT_POINT * d_s;

        let d_e = util::rand_scalar();
        let q_e = RISTRETTO_BASEPOINT_POINT * d_e;

        let d_r = util::rand_scalar();
        let q_r = RISTRETTO_BASEPOINT_POINT * d_r;

        (d_s, q_s, d_e, q_e, d_r, q_r)
    }
}
