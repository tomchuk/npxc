//! `WireGuard` key material for the egress tunnel.
//!
//! npxc generates two keypairs per session — one for itself, one for the guest
//! — and injects the guest's private key plus npxc's public key into the
//! container. The keys are ephemeral and discarded at session teardown.

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use boringtun::x25519::{PublicKey, StaticSecret};

/// A `WireGuard` `Curve25519` keypair.
///
/// Wraps `boringtun`'s re-exported `X25519` types so the key material is
/// guaranteed compatible with [`boringtun::noise::Tunn`].
pub struct WgKeypair {
    secret: StaticSecret,
    public: PublicKey,
}

impl WgKeypair {
    /// Generate a fresh random keypair from the operating system CSPRNG.
    ///
    /// # Panics
    ///
    /// Panics only if the OS random source is unavailable, which on supported
    /// platforms effectively never happens after early boot.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("OS random source unavailable");
        Self::from_secret_bytes(bytes)
    }

    /// Construct a keypair from raw 32-byte private-key material, deriving the
    /// public key.
    #[must_use]
    pub fn from_secret_bytes(bytes: [u8; 32]) -> Self {
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// The `X25519` static secret (for the local side of [`boringtun::noise::Tunn::new`]).
    #[must_use]
    pub fn secret(&self) -> &StaticSecret {
        &self.secret
    }

    /// The `X25519` public key (for the peer side of [`boringtun::noise::Tunn::new`]).
    #[must_use]
    pub fn public(&self) -> &PublicKey {
        &self.public
    }

    /// Base64-encoded private key, in `WireGuard` `PrivateKey` config format.
    #[must_use]
    pub fn private_base64(&self) -> String {
        B64.encode(self.secret.to_bytes())
    }

    /// Base64-encoded public key, in `WireGuard` `PublicKey` config format.
    #[must_use]
    pub fn public_base64(&self) -> String {
        B64.encode(self.public.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_distinct_keypairs() {
        let a = WgKeypair::generate();
        let b = WgKeypair::generate();
        assert_ne!(a.private_base64(), b.private_base64());
        assert_ne!(a.public_base64(), b.public_base64());
    }

    #[test]
    fn public_is_deterministic_from_secret() {
        let bytes = [7u8; 32];
        let a = WgKeypair::from_secret_bytes(bytes);
        let b = WgKeypair::from_secret_bytes(bytes);
        assert_eq!(a.public_base64(), b.public_base64());
    }

    #[test]
    fn distinct_secrets_yield_distinct_public_keys() {
        let a = WgKeypair::from_secret_bytes([1u8; 32]);
        let b = WgKeypair::from_secret_bytes([2u8; 32]);
        assert_ne!(a.public_base64(), b.public_base64());
    }

    #[test]
    fn base64_keys_decode_to_32_bytes() {
        let kp = WgKeypair::generate();
        assert_eq!(B64.decode(kp.private_base64()).unwrap().len(), 32);
        assert_eq!(B64.decode(kp.public_base64()).unwrap().len(), 32);
    }

    #[test]
    fn private_key_round_trips_through_base64() {
        // Encoding the private key and reconstructing from it must reproduce
        // the same public key — proving the encode/derive pipeline is sound.
        let kp = WgKeypair::generate();
        let decoded: [u8; 32] = B64.decode(kp.private_base64()).unwrap().try_into().unwrap();
        let restored = WgKeypair::from_secret_bytes(decoded);
        assert_eq!(kp.public_base64(), restored.public_base64());
    }
}
