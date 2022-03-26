//! A multi-recipient, hybrid cryptosystem.

use std::convert::TryInto;
use std::io::{self, Read, Write};
use std::mem;
use std::result::Result;

use curve25519_dalek::ristretto::RistrettoPoint;
use rand::{CryptoRng, Rng};

use crate::duplex::{Duplex, TAG_LEN};
use crate::ristretto::{CanonicallyEncoded, G, POINT_LEN};
use crate::ristretto::{Point, Scalar};
use crate::schnorr::SIGNATURE_LEN;
use crate::sres::NONCE_LEN;
use crate::{schnorr, sres, DecryptError};

/// Encrypt the contents of `reader` such that they can be decrypted and verified by all members of
/// `q_rs` and write the ciphertext to `writer` with `padding` bytes of random data added.
pub fn encrypt(
    mut rng: impl Rng + CryptoRng,
    reader: &mut impl Read,
    writer: &mut impl Write,
    (d_s, q_s): (&Scalar, &Point),
    q_rs: &[Point],
    padding: u64,
) -> io::Result<u64> {
    let mut written = 0;

    // Initialize a duplex and absorb the sender's public key.
    let mut mres = Duplex::new("veil.mres");
    mres.absorb(&q_s.to_canonical_encoding());

    // Derive a random ephemeral key pair, commitment scalar, and data encryption key from the
    // duplex's current state, the sender's private key, and a random nonce. Loop until we produce
    // an ephemeral private key which has a public key with an Elligator2 representative.
    let mut generate_keys = || loop {
        let (d_e, k, dek) = mres.hedge(&mut rng, d_s, |clone| {
            (clone.squeeze_scalar(), clone.squeeze_scalar(), clone.squeeze(DEK_LEN))
        });
        let q_e = &d_e * &G;
        if let Some(q_e_r) = q_e.to_elligator2() {
            return (d_e, q_e, q_e_r, k, dek);
        }
    };
    let (d_e, q_e, q_e_r, k, dek) = generate_keys();

    // Absorb and write the representative of the ephemeral public key.
    mres.absorb(&q_e_r);
    writer.write_all(&q_e_r)?;
    written += POINT_LEN as u64;

    // Encrypt a header for each recipient.
    written += encrypt_headers(
        &mut mres,
        &mut rng,
        (d_s, q_s),
        (&d_e, &q_e),
        q_rs,
        &dek,
        padding,
        writer,
    )?;

    // Add random padding to the end of the headers.
    let mut mres = mres.absorb_stream(writer);
    io::copy(&mut RngRead(&mut rng).take(padding), &mut mres)?;
    let (mut mres, mut writer, _) = mres.into_inner()?;

    // Absorb the DEK.
    mres.absorb(&dek);

    // Encrypt the plaintext in blocks and write them.
    written += encrypt_message(&mut mres, reader, &mut writer)?;

    // Sign the duplex's final state with the ephemeral private key.
    let (i, s) = schnorr::sign_duplex(&mut mres, &d_e, k);

    // Encrypt the proof scalar.
    let s = mres.encrypt(&s.to_canonical_encoding());

    // Write the signature components.
    writer.write_all(&i)?;
    writer.write_all(&s)?;

    Ok(written + padding + i.len() as u64 + s.len() as u64)
}

#[allow(clippy::too_many_arguments)]
fn encrypt_headers(
    mres: &mut Duplex,
    mut rng: impl Rng + CryptoRng,
    (d_s, q_s): (&Scalar, &Point),
    (d_e, q_e): (&Scalar, &Point),
    q_rs: &[Point],
    dek: &[u8],
    padding: u64,
    writer: &mut impl Write,
) -> io::Result<u64> {
    let mut written = 0u64;

    // Encode the DEK, recipient count, and padding count in a header.
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend(dek);
    header.extend((q_rs.len() as u64).to_le_bytes());
    header.extend(padding.to_le_bytes());

    // For each recipient, encrypt a copy of the header with veil.sres.
    for q_r in q_rs {
        // Squeeze a nonce for each header.
        let nonce = mres.squeeze(NONCE_LEN);

        // Encrypt the header for the given recipient.
        let ciphertext = sres::encrypt(&mut rng, (d_s, q_s), (d_e, q_e), q_r, &nonce, &header);

        // Absorb the encrypted header.
        mres.absorb(&ciphertext);

        // Write the encrypted header.
        writer.write_all(&ciphertext)?;
        written += ciphertext.len() as u64;
    }

    Ok(written)
}

fn encrypt_message(
    mres: &mut Duplex,
    reader: &mut impl Read,
    writer: &mut impl Write,
) -> io::Result<u64> {
    let mut buf = Vec::with_capacity(BLOCK_LEN);
    let mut written = 0;

    loop {
        // Read a block of data.
        let n = reader.take(BLOCK_LEN as u64).read_to_end(&mut buf)?;
        let block = &buf[..n];

        // Encrypt the block and write the ciphertext and a tag.
        writer.write_all(&mres.seal(block))?;
        written += (n + TAG_LEN) as u64;

        // Ratchet the duplex state to prevent rollback. This protects previous blocks from being
        // reversed in the event of the duplex's state being compromised.
        mres.ratchet();

        // If the block was undersized, we're at the end of the reader.
        if n < BLOCK_LEN {
            break;
        }

        // Reset the buffer.
        buf.clear();
    }

    Ok(written)
}

/// Decrypt the contents of `reader` iff they were originally encrypted by `q_s` for `q_r` and write
/// the plaintext to `writer`.
pub fn decrypt(
    reader: &mut impl Read,
    writer: &mut impl Write,
    (d_r, q_r): (&Scalar, &Point),
    q_s: &Point,
) -> Result<u64, DecryptError> {
    // Initialize a duplex and absorb the sender's public key.
    let mut mres = Duplex::new("veil.mres");
    mres.absorb(&q_s.to_canonical_encoding());

    // Read, absorb, and decode the ephemeral public key.
    let mut q_e_r = [0u8; POINT_LEN];
    reader.read_exact(&mut q_e_r)?;
    mres.absorb(&q_e_r);
    let q_e = RistrettoPoint::from_elligator2(&q_e_r).ok_or(DecryptError::InvalidCiphertext)?;

    // Find a header, decrypt it, and write the entirety of the headers and padding to the duplex.
    let (mut mres, dek) = decrypt_header(mres, reader, d_r, q_r, &q_e, q_s)?;

    // Absorb the DEK.
    mres.absorb(&dek);

    // Decrypt the message and verify the signature.
    decrypt_message(&mut mres, &q_e, reader, writer)
}

fn decrypt_message(
    mres: &mut Duplex,
    q_e: &Point,
    reader: &mut impl Read,
    writer: &mut impl Write,
) -> Result<u64, DecryptError> {
    let mut buf = Vec::with_capacity(ENC_BLOCK_LEN + SIGNATURE_LEN);
    let mut written = 0;

    loop {
        // Read a block and a possible signature, keeping in mind the unused bit of the buffer from
        // the last iteration.
        let n = reader
            .take((ENC_BLOCK_LEN + SIGNATURE_LEN - buf.len()) as u64)
            .read_to_end(&mut buf)?;

        // If we're at the end of the reader, we only have the signature left to process. Break out
        // of the read loop and go process the signature.
        if n == 0 {
            break;
        }

        // Pretend we don't see the possible signature at the end.
        let n = buf.len() - SIGNATURE_LEN;
        let block = &buf[..n];

        // Decrypt the block and write the plaintext. If the block cannot be decrypted, return an
        // error.
        let plaintext = mres.unseal(block).ok_or(DecryptError::InvalidCiphertext)?;
        writer.write_all(&plaintext)?;
        written += plaintext.len() as u64;

        // Ratchet the duplex state.
        mres.ratchet();

        // Clear the part of the buffer we used.
        buf.drain(0..n);
    }

    // Verify the signature.
    schnorr::verify_duplex(mres, q_e, &buf).map_err(|_| DecryptError::InvalidCiphertext)?;

    // Return the number of bytes.
    Ok(written)
}

fn decrypt_header(
    mut mres: Duplex,
    reader: &mut impl Read,
    d_r: &Scalar,
    q_r: &Point,
    q_e: &Point,
    q_s: &Point,
) -> Result<(Duplex, Vec<u8>), DecryptError> {
    let mut buf = Vec::with_capacity(ENC_HEADER_LEN);
    let mut i = 0u64;
    let mut hdr_count = u64::MAX;

    let mut padding: Option<u64> = None;
    let mut dek: Option<Vec<u8>> = None;

    // Iterate through blocks, looking for an encrypted header that can be decrypted.
    while i < hdr_count {
        // Read a potential encrypted header.
        let n = reader.take(ENC_HEADER_LEN as u64).read_to_end(&mut buf)?;
        let header = &buf[..n];

        // If the header is short, we're at the end of the reader.
        if header.len() < ENC_HEADER_LEN {
            return Err(DecryptError::InvalidCiphertext);
        }

        // Squeeze a nonce regardless of whether we need to in order to keep the duplex state
        // consistent.
        let nonce = mres.squeeze(NONCE_LEN);

        // If a header hasn't been decrypted yet, try to decrypt this one.
        if dek.is_none() {
            if let Some((d, c, p)) =
                sres::decrypt((d_r, q_r), q_e, q_s, &nonce, header).and_then(decode_header)
            {
                // If the header was successfully decrypted, keep the DEK and padding and update the
                // loop variable to not be effectively infinite.
                dek = Some(d);
                hdr_count = c;
                padding = Some(p);
            }
        }

        // Absorb the encrypted header with the duplex.
        mres.absorb(header);
        i += 1;

        buf.clear();
    }

    if let Some((dek, padding)) = dek.zip(padding) {
        // Read the remainder of the padding and absorb it with the duplex.
        let mut mres = mres.absorb_stream(io::sink());
        let mut remainder = reader.take(padding);
        io::copy(&mut remainder, &mut mres)?;

        // Unwrap the writer.
        let (mres, _, _) = mres.into_inner()?;

        // Return the duplex and DEK.
        Ok((mres, dek))
    } else {
        Err(DecryptError::InvalidCiphertext)
    }
}

#[inline]
fn decode_header(header: Vec<u8>) -> Option<(Vec<u8>, u64, u64)> {
    // Check header for proper length.
    if header.len() != HEADER_LEN {
        return None;
    }

    // Split header into components.
    let (dek, hdr_count) = header.split_at(DEK_LEN);
    let (hdr_count, padding) = hdr_count.split_at(mem::size_of::<u64>());

    // Decode components.
    let dek = dek.to_vec();
    let hdr_count = u64::from_le_bytes(hdr_count.try_into().expect("invalid u64 len"));
    let padding = u64::from_le_bytes(padding.try_into().expect("invalid u64 len"));

    Some((dek, hdr_count, padding))
}

struct RngRead<R>(R)
where
    R: Rng + CryptoRng;

impl<R> Read for RngRead<R>
where
    R: Rng + CryptoRng,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.try_fill_bytes(buf)?;
        Ok(buf.len())
    }
}

const DEK_LEN: usize = 32;
const HEADER_LEN: usize = DEK_LEN + mem::size_of::<u64>() + mem::size_of::<u64>();
const ENC_HEADER_LEN: usize = HEADER_LEN + sres::OVERHEAD;
const BLOCK_LEN: usize = 32 * 1024;
const ENC_BLOCK_LEN: usize = BLOCK_LEN + TAG_LEN;

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use rand::SeedableRng;
    use rand_chacha::ChaChaRng;

    use super::*;

    macro_rules! assert_failed {
        ($action: expr) => {
            match $action {
                Ok(_) => panic!("decrypted but shouldn't have"),
                Err(DecryptError::InvalidCiphertext) => Ok(()),
                Err(e) => Err(e),
            }
        };
    }

    #[test]
    fn round_trip() -> Result<(), DecryptError> {
        let (mut rng, d_s, q_s, d_r, q_r) = setup();

        let message = b"this is a thingy";
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        let q_rs = &[q_s, q_r, q_s, q_s, q_s];
        let ctx_len = encrypt(&mut rng, &mut src, &mut dst, (&d_s, &q_s), q_rs, 123)?;
        assert_eq!(dst.position(), ctx_len, "returned/observed ciphertext length mismatch");

        let mut src = Cursor::new(dst.into_inner());
        let mut dst = Cursor::new(Vec::new());

        let ptx_len = decrypt(&mut src, &mut dst, (&d_r, &q_r), &q_s)?;
        assert_eq!(dst.position(), ptx_len, "returned/observed plaintext length mismatch");
        assert_eq!(message.to_vec(), dst.into_inner(), "incorrect plaintext");

        Ok(())
    }

    #[test]
    fn bad_sender_public_key() -> Result<(), DecryptError> {
        let (mut rng, d_s, q_s, d_r, q_r) = setup();

        let message = b"this is a thingy";
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        let ctx_len = encrypt(&mut rng, &mut src, &mut dst, (&d_s, &q_s), &[q_s, q_r], 123)?;
        assert_eq!(dst.position(), ctx_len, "returned/observed ciphertext length mismatch");

        let q_s = Point::random(&mut rng);

        let mut src = Cursor::new(dst.into_inner());
        let mut dst = Cursor::new(Vec::new());

        assert_failed!(decrypt(&mut src, &mut dst, (&d_r, &q_r), &q_s))
    }

    #[test]
    fn bad_recipient_public_key() -> Result<(), DecryptError> {
        let (mut rng, d_s, q_s, d_r, q_r) = setup();

        let message = b"this is a thingy";
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        let ctx_len = encrypt(&mut rng, &mut src, &mut dst, (&d_s, &q_s), &[q_s, q_r], 123)?;
        assert_eq!(dst.position(), ctx_len, "returned/observed ciphertext length mismatch");

        let q_r = Point::random(&mut rng);

        let mut src = Cursor::new(dst.into_inner());
        let mut dst = Cursor::new(Vec::new());

        assert_failed!(decrypt(&mut src, &mut dst, (&d_r, &q_r), &q_s))
    }

    #[test]
    fn bad_recipient_private_key() -> Result<(), DecryptError> {
        let (mut rng, d_s, q_s, _, q_r) = setup();

        let message = b"this is a thingy";
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        let ctx_len = encrypt(&mut rng, &mut src, &mut dst, (&d_s, &q_s), &[q_s, q_r], 123)?;
        assert_eq!(dst.position(), ctx_len, "returned/observed ciphertext length mismatch");

        let d_r = Scalar::random(&mut rng);

        let mut src = Cursor::new(dst.into_inner());
        let mut dst = Cursor::new(Vec::new());

        assert_failed!(decrypt(&mut src, &mut dst, (&d_r, &q_r), &q_s))
    }

    #[test]
    fn multi_block_message() -> Result<(), DecryptError> {
        let (mut rng, d_s, q_s, d_r, q_r) = setup();

        let message = [69u8; 65 * 1024];
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        let ctx_len = encrypt(&mut rng, &mut src, &mut dst, (&d_s, &q_s), &[q_s, q_r], 123)?;
        assert_eq!(dst.position(), ctx_len, "returned/observed ciphertext length mismatch");

        let mut src = Cursor::new(dst.into_inner());
        let mut dst = Cursor::new(Vec::new());

        let ptx_len = decrypt(&mut src, &mut dst, (&d_r, &q_r), &q_s)?;
        assert_eq!(dst.position(), ptx_len, "returned/observed plaintext length mismatch");
        assert_eq!(message.to_vec(), dst.into_inner(), "incorrect plaintext");

        Ok(())
    }

    #[test]
    fn split_sig() -> Result<(), DecryptError> {
        let (mut rng, d_s, q_s, d_r, q_r) = setup();

        let message = [69u8; 32 * 1024 - 37];
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        let ctx_len = encrypt(&mut rng, &mut src, &mut dst, (&d_s, &q_s), &[q_s, q_r], 0)?;
        assert_eq!(dst.position(), ctx_len, "returned/observed ciphertext length mismatch");

        let mut src = Cursor::new(dst.into_inner());
        let mut dst = Cursor::new(Vec::new());

        let ptx_len = decrypt(&mut src, &mut dst, (&d_r, &q_r), &q_s)?;
        assert_eq!(dst.position(), ptx_len, "returned/observed plaintext length mismatch");
        assert_eq!(message.to_vec(), dst.into_inner(), "incorrect plaintext");

        Ok(())
    }

    #[test]
    fn flip_every_bit() -> Result<(), DecryptError> {
        let (mut rng, d_s, q_s, d_r, q_r) = setup();

        let message = b"this is a thingy";
        let mut src = Cursor::new(message);
        let mut dst = Cursor::new(Vec::new());

        encrypt(&mut rng, &mut src, &mut dst, (&d_s, &q_s), &[q_s, q_r], 123)?;

        let ciphertext = dst.into_inner();

        for i in 0..ciphertext.len() {
            for j in 0u8..8 {
                let mut ciphertext = ciphertext.clone();
                ciphertext[i] ^= 1 << j;
                let mut src = Cursor::new(ciphertext);

                match decrypt(&mut src, &mut io::sink(), (&d_r, &q_r), &q_s) {
                    Err(DecryptError::InvalidCiphertext) => {}
                    Ok(_) => panic!("bit flip at byte {i}, bit {j} produced a valid message"),
                    Err(e) => panic!("unknown error: {}", e),
                };
            }
        }

        Ok(())
    }

    fn setup() -> (ChaChaRng, Scalar, Point, Scalar, Point) {
        let mut rng = ChaChaRng::seed_from_u64(0xDEADBEEF);

        let d_s = Scalar::random(&mut rng);
        let q_s = &d_s * &G;

        let d_r = Scalar::random(&mut rng);
        let q_r = &d_r * &G;

        (rng, d_s, q_s, d_r, q_r)
    }
}
