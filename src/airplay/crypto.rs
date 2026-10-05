//! The few primitives CarPlay leans on.
//!
//! Everything here mirrors LIVI's TypeScript crypto one-for-one, because the
//! phone only accepts the exact constructions below:
//!
//! | Purpose              | Construction                                        |
//! |----------------------|-----------------------------------------------------|
//! | key derivation       | HKDF-SHA512 with ASCII salt and info strings        |
//! | media + control seal | ChaCha20-Poly1305, 12-byte nonce                    |
//! | pair-verify ECDH     | X25519, with an Ed25519 signature proving identity  |
//! | pair-setup           | SRP-6a over the RFC 5054 3072-bit group, SHA-512    |
//!
//! Two nonce shapes matter, and mixing them up is the classic reason every
//! frame decrypts to garbage, so both are spelled out here.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use sha2::{Digest, Sha256, Sha512};
use x25519_dalek::{PublicKey, StaticSecret};

pub type Result32 = Result<[u8; 32], CryptoError>;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("bad key length")]
    BadKey,
    #[error("aead failure (tampered data, wrong key, or off-by-one counter)")]
    Aead,
    #[error("bad signature")]
    BadSignature,
    #[error("bad public key")]
    BadPublicKey,
}

/// Framing a failure reaches the same call sites a socket error does, so let it
/// travel as one instead of being mapped at each of them.
impl From<CryptoError> for std::io::Error {
    fn from(e: CryptoError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    }
}

/// A ChaCha20-Poly1305 key.
#[derive(Clone)]
pub struct SymKey(pub Key);

impl SymKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != 32 {
            return Err(CryptoError::BadKey);
        }
        Ok(Self(*Key::from_slice(bytes)))
    }
}

/// HKDF-SHA512, extracting with `salt` and expanding with `info`.
///
/// Note that the CarPlay strings are plain ASCII and are used verbatim (no
/// trailing NUL, no hex decoding).
pub fn hkdf_sha512(ikm: &[u8], salt: &str, info: &str, len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha512>::new(Some(salt.as_bytes()), ikm);
    let mut out = vec![0u8; len];
    hk.expand(info.as_bytes(), &mut out)
        .expect("32/64 byte expansions are always in range");
    out
}

/// The 32-byte convenience wrapper used all over the handshake.
pub fn hkdf32(ikm: &[u8], salt: &str, info: &str) -> [u8; 32] {
    let v = hkdf_sha512(ikm, salt, info, 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    out
}

/// Media and per-frame nonce: four zero bytes then the counter little-endian.
///
/// The counter starts at zero and increments once per successfully opened
/// message. A failed open must NOT bump it, otherwise a single lost frame
/// desynchronises the stream for good.
pub fn nonce_counter(counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n
}

/// Handshake nonce: the label right-aligned in twelve bytes, zero padded.
/// Used for the pair-setup message encryption (`PS-Msg05`, `PS-Msg06`).
pub fn nonce_label(label: &str) -> [u8; 12] {
    let mut n = [0u8; 12];
    let b = label.as_bytes();
    if b.len() > 12 {
        panic!("label too long: {label}");
    }
    n[12 - b.len()..].copy_from_slice(b);
    n
}

fn to_nonce(n: &[u8; 12]) -> Nonce {
    *Nonce::from_slice(n)
}

/// Opens a sealed message, treating `aad` as additional authenticated data.
pub fn open(
    key: &SymKey,
    nonce: &[u8; 12],
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = ChaCha20Poly1305::new(&key.0);
    cipher
        .decrypt(
            &to_nonce(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Aead)
}

/// Seals a message the same way.
pub fn seal(
    key: &SymKey,
    nonce: &[u8; 12],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = ChaCha20Poly1305::new(&key.0);
    cipher
        .encrypt(
            &to_nonce(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Aead)
}

// -------------------------------------------------------------- framing ----

/// The ChaCha20-Poly1305 tag that follows the ciphertext in every framed
/// record, and is not counted by the two-byte length in front of it.
pub const TAG_LEN: usize = 16;

/// The largest plaintext the phone will take in one frame. The control channel
/// and the event channel frame the same way, so they share the limit.
const MAX_PAYLOAD: usize = 0x4000;

/// Seals `plain` as one or more `[2B length][ciphertext][tag]` records,
/// advancing `counter` once per record.
///
/// The length doubles as the additional authenticated data, so it has to be
/// settled before the frame is sealed — which is the whole reason this lives
/// here rather than being spelled out at each place that writes frames.
pub fn seal_frames(key: &SymKey, counter: &mut u64, plain: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut out = Vec::new();
    for chunk in plain.chunks(MAX_PAYLOAD) {
        let header = (chunk.len() as u16).to_le_bytes();
        let sealed = seal(key, &nonce_counter(*counter), chunk, &header)?;
        *counter += 1;
        out.extend_from_slice(&header);
        out.extend_from_slice(&sealed);
    }
    Ok(out)
}

/// The read direction: opens every complete record at the front of `buf` and
/// appends the plaintext to `plain`, leaving a partial record behind for the
/// next read.
///
/// The counter advances only on a successful open. One frame that does not
/// open must not shift every frame after it.
pub fn open_frames(
    key: &SymKey,
    counter: &mut u64,
    buf: &mut Vec<u8>,
    plain: &mut Vec<u8>,
) -> Result<(), CryptoError> {
    while buf.len() >= 2 {
        let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        let total = 2 + len + TAG_LEN;
        if buf.len() < total {
            break;
        }
        let aad = buf[..2].to_vec();
        let sealed = buf[2..total].to_vec();
        buf.drain(..total);
        let opened = open(key, &nonce_counter(*counter), &sealed, &aad)?;
        *counter += 1;
        plain.extend_from_slice(&opened);
    }
    Ok(())
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    use sha1::Digest;
    let mut out = [0u8; 20];
    out.copy_from_slice(&sha1::Sha1::digest(data));
    out
}

/// Concatenated SHA-1, the way /auth-setup derives its AES key and IV.
pub fn sha1_all(parts: &[&[u8]]) -> [u8; 20] {
    use sha1::Digest;
    let mut h = sha1::Sha1::new();
    for p in parts {
        h.update(p);
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&h.finalize());
    out
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// Concatenated SHA-256, which is the /auth-setup digest on 3.0 accessories.
pub fn sha256_all(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// AES-128-CTR, which is its own inverse — /auth-setup uses it to hide the
/// coprocessor signature from anything sniffing the session keys.
pub fn aes_ctr128(key: &[u8], iv: &[u8], data: &[u8]) -> Vec<u8> {
    use aes::cipher::{Key, KeyIvInit, StreamCipher};
    let mut cipher = ctr::Ctr128BE::<aes::Aes128>::new(
        Key::<aes::Aes128>::from_slice(key),
        Key::<aes::Aes128>::from_slice(iv),
    );
    let mut buf = data.to_vec();
    cipher.apply_keystream(&mut buf);
    buf
}

pub fn sha512(data: &[u8]) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(data);
    let mut out = [0u8; 64];
    out.copy_from_slice(&h.finalize());
    out
}

/// Concatenated SHA-512 over several pieces.
pub fn sha512_all(parts: &[&[u8]]) -> [u8; 64] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    let mut out = [0u8; 64];
    out.copy_from_slice(&h.finalize());
    out
}

// ---------------------------------------------------------------- X25519 ----

pub struct Ephemeral {
    secret: StaticSecret,
    pub public: [u8; 32],
}

impl Ephemeral {
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        let secret = StaticSecret::from(seed);
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    /// Diffie-Hellman against the peer's 32-byte public key.
    pub fn shared(&self, peer: &[u8; 32]) -> [u8; 32] {
        let their = PublicKey::from(*peer);
        *self.secret.diffie_hellman(&their).as_bytes()
    }
}

// --------------------------------------------------------------- Ed25519 ----

/// The accessory's long-term key. Generated once and persisted, because the
/// phone remembers the public half and will reject a stranger.
#[derive(Clone)]
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(seed),
        }
    }

    /// `ed25519_dalek` 2 dropped `SigningKey::generate` (it wants a full CSPRNG
    /// rather than the tuple `rand` hands out), so roll the seed ourselves and
    /// build the key from it — which is the same thing it used to do internally.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn public(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }
}

pub fn ed25519_verify(public: &[u8; 32], msg: &[u8], sig: &[u8]) -> Result<(), CryptoError> {
    let key = VerifyingKey::from_bytes(public).map_err(|_| CryptoError::BadPublicKey)?;
    let sig = Signature::from_slice(sig).map_err(|_| CryptoError::BadSignature)?;
    key.verify(msg, &sig).map_err(|_| CryptoError::BadSignature)
}
