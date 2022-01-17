use std::mem;

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_TABLE;
use curve25519_dalek::ristretto::RistrettoBasepointTable;
use rand::RngCore;

/// Generate a random `u8` array.
#[must_use]
pub fn rand_array<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    rand::thread_rng().fill_bytes(&mut out);
    out
}

/// The generator point for ristretto255.
pub const G: &RistrettoBasepointTable = &RISTRETTO_BASEPOINT_TABLE;

/// The length of a MAC in bytes.
pub const MAC_LEN: usize = 16;

/// The length of a compressed ristretto255 point in bytes.
pub const POINT_LEN: usize = 32;

/// The length of a `u32` in bytes.
pub const U32_LEN: usize = mem::size_of::<u32>();

/// The length of a `u64` in bytes.
pub const U64_LEN: usize = mem::size_of::<u64>();
