//! Implements the static ecdh algorithm required by discv5 in terms of the `k256` library.
use super::k256::{
    self,
    ecdsa::{SigningKey, VerifyingKey},
    elliptic_curve::sec1::ToEncodedPoint,
};

pub fn ecdh(public_key: &VerifyingKey, secret_key: &SigningKey) -> Vec<u8> {
    k256::PublicKey::from_affine(
        (k256::PublicKey::from_sec1_bytes(public_key.to_sec1_bytes().as_ref())
            .unwrap()
            .to_projective()
            * k256::SecretKey::from_slice(&secret_key.to_bytes())
                .unwrap()
                .to_nonzero_scalar()
                .as_ref())
        .to_affine(),
    )
    .unwrap()
    .to_encoded_point(true)
    .as_bytes()
    .to_vec()
}
