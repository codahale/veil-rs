//! Scalar derivation functions.

use crate::duplex::Duplex;
use crate::ristretto::G;
use crate::ristretto::{Point, Scalar};

/// Derive a scalar from the given secret key.
#[must_use]
pub fn derive_root(r: &[u8]) -> Scalar {
    // Initialize the duplex.
    let mut root_df = Duplex::new("veil.scaldf.root");

    // Absorb the secret key.
    root_df.absorb(r);

    // Squeeze a scalar.
    root_df.squeeze_scalar()
}

/// Derive a scalar from another scalar using the given key ID.
#[must_use]
pub fn derive_scalar(d: &Scalar, key_id: &str) -> Scalar {
    key_id.trim_matches(KEY_ID_DELIM).split(KEY_ID_DELIM).fold(*d, |d, label| {
        // Initialize the duplex.
        let mut label_df = Duplex::new("veil.scaldf.label");

        // Absorb the label.
        label_df.absorb(label.as_bytes());

        // Squeeze a scalar.
        d + label_df.squeeze_scalar()
    })
}

/// Derive a point from another point using the given key ID.
#[must_use]
pub fn derive_point(q: &Point, key_id: &str) -> Point {
    q + (&derive_scalar(&Scalar::zero(), key_id) * &G)
}

const KEY_ID_DELIM: char = '/';

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_derivation() {
        let d = Scalar::random(&mut rand::thread_rng());
        let d1 = derive_scalar(&d, "/one");
        let d2 = derive_scalar(&d1, "/two");
        let d3 = derive_scalar(&d2, "/three");

        let d3_p = derive_scalar(&d, "/one/two/three");

        assert_eq!(d3_p, d3, "invalid hierarchical derivation");
    }

    #[test]
    fn point_derivation() {
        let d = Scalar::random(&mut rand::thread_rng());
        let q = &d * &G;

        let q1 = derive_point(&q, "/one");
        let q2 = derive_point(&q1, "/two");
        let q3 = derive_point(&q2, "/three");

        let q3_p = derive_point(&q, "/one/two/three");

        assert_eq!(q3_p, q3, "invalid hierarchical derivation");
    }
}
