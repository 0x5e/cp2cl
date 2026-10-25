//! Pairing: SRP-6a first contact, then X25519 + Ed25519 for every later session.
//!
//! Two different protocols wear the same name here:
//!
//! * `pair-setup` — one-time, a PIN-less SRP-6a exchange that ends with an
//!   exchange of long-term Ed25519 public keys. The PIN is the fixed `3939`
//!   (which is what every CarPlay accessory uses; the phone never prompts).
//! * `pair-verify` — every session, an ephemeral X25519 exchange. Its shared
//!   secret is the root every other key in the session derives from.
//!
//! Persisting the accessory Ed25519 seed matters more than it looks: the phone
//! stores our public key, so a freshly generated identity reads a stranger and
//! the phone asks to be paired again (or refuses outright).

use num_bigint::BigUint;
use std::path::PathBuf;

use super::crypto::{
    self, ed25519_verify, hkdf32, hkdf_sha512, nonce_label, open, seal, Identity, SymKey,
};

// ------------------------------------------------------------------ TLV8 ----

// In wire order, minus the two we neither send nor read: 0x00 (method) and
// 0x07 (error).
pub const TYPE_IDENTIFIER: u8 = 0x01;
pub const TYPE_SALT: u8 = 0x02;
pub const TYPE_PUBLIC_KEY: u8 = 0x03;
pub const TYPE_PROOF: u8 = 0x04;
pub const TYPE_ENCRYPTED_DATA: u8 = 0x05;
pub const TYPE_STATE: u8 = 0x06;
pub const TYPE_SIGNATURE: u8 = 0x0a;

pub type Tlv = Vec<(u8, Vec<u8>)>;

/// Encodes TLV8. Values longer than 255 bytes are emitted as repeated entries
/// of the same type, which receivers concatenate.
pub fn tlv_encode(items: &Tlv) -> Vec<u8> {
    let mut out = Vec::new();
    for (t, v) in items {
        let mut rest = &v[..];
        if rest.is_empty() {
            out.push(*t);
            out.push(0);
            continue;
        }
        loop {
            let n = rest.len().min(255);
            out.push(*t);
            out.push(n as u8);
            out.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            if rest.is_empty() {
                break;
            }
        }
    }
    out
}

/// Concatenates repeated type entries, which is what every caller wants.
pub fn tlv_get(items: &Tlv, want: u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut found = false;
    for (t, v) in items {
        if *t == want {
            out.extend_from_slice(v);
            found = true;
        }
    }
    if found {
        Some(out)
    } else {
        None
    }
}

pub fn tlv_state(items: &Tlv) -> Option<u8> {
    tlv_get(items, TYPE_STATE).and_then(|v| v.first().copied())
}

pub fn tlv_parse(bytes: &[u8]) -> Tlv {
    let mut out = Tlv::new();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let t = bytes[i];
        let len = bytes[i + 1] as usize;
        if i + 2 + len > bytes.len() {
            break;
        }
        out.push((t, bytes[i + 2..i + 2 + len].to_vec()));
        i += 2 + len;
    }
    out
}

// ------------------------------------------------------------ pair storage --

/// Where the accessory identity and remembered controllers live.
pub struct Pairings {
    path: PathBuf,
    pub accessory_id: String,
    pub identity: Identity,
}

impl Pairings {
    pub fn load_or_create() -> std::io::Result<Self> {
        let path = crate::config::data_dir()?.join("pairings.json");

        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                let id = v["accessory_id"].as_str().unwrap_or("").to_string();
                if let Some(seed) = v["seed"].as_str().and_then(|s| hex::decode(s).ok()) {
                    if seed.len() == 32 && !id.is_empty() {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&seed);
                        return Ok(Self {
                            path,
                            accessory_id: id,
                            identity: Identity::from_seed(&arr),
                        });
                    }
                }
            }
        }

        let identity = Identity::generate();
        let mut id_bytes = [0u8; 6];
        getrandom_fill(&mut id_bytes);
        let acc = format!("{}", hex::encode(id_bytes));
        let s = Self {
            path,
            accessory_id: acc,
            identity,
        };
        s.save()?;
        Ok(s)
    }

    fn save(&self) -> std::io::Result<()> {
        let v = serde_json::json!({
            "accessory_id": self.accessory_id,
            "seed": hex::encode(self.identity.seed()),
        });
        std::fs::write(&self.path, v.to_string())
    }
}

fn getrandom_fill(buf: &mut [u8]) {
    use rand::RngCore;
    rand::thread_rng().fill_bytes(buf);
}

// ------------------------------------------------------------- pair-setup ---

/// RFC 5054, 3072-bit group. Every byte of this matters.
const N_HEX: &str = "\
FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DD\
EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED\
EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F\
83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B\
E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA0510\
15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7\
ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B18177B200C\
BBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";

const N_BYTES: usize = 384;
const USERNAME: &str = "Pair-Setup";
const PIN: &str = "3939";

fn n_prime() -> BigUint {
    BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("group prime is valid hex")
}

fn g() -> BigUint {
    BigUint::from(5u8)
}

/// Left-pads to the group size, which is how SRP hashes byte strings.
fn pad(value: &BigUint) -> [u8; N_BYTES] {
    let bytes = value.to_bytes_be();
    let mut out = [0u8; N_BYTES];
    let off = N_BYTES - bytes.len().min(N_BYTES);
    out[off..].copy_from_slice(&bytes[bytes.len().min(N_BYTES) - (N_BYTES - off)..]);
    out
}

fn to_big(v: &[u8]) -> BigUint {
    BigUint::from_bytes_be(v)
}

pub struct PairSetup {
    salt: [u8; 16],
    private: BigUint,
    pub public_b: Vec<u8>,
    // Set once the phone answers M2.
    session_key: Option<[u8; 64]>,
}

impl Default for PairSetup {
    fn default() -> Self {
        Self::new()
    }
}

impl PairSetup {
    pub fn new() -> Self {
        let n = n_prime();
        let g = g();
        // k = H(N || pad(g)) — the SRP-6a multiplier, computed exactly once.
        let k = to_big(&crypto::sha512_all(&[&n.to_bytes_be(), &pad(&g)]));

        let mut salt = [0u8; 16];
        getrandom_fill(&mut salt);

        let x = to_big(&crypto::sha512_all(&[
            &salt,
            &crypto::sha512_all(&[USERNAME.as_bytes(), b":", PIN.as_bytes()]),
        ]));
        let verifier = g.modpow(&x, &n);

        let mut raw = [0u8; 32];
        getrandom_fill(&mut raw);
        let private = to_big(&raw);
        let public = (k * &verifier + g.modpow(&private, &n)) % &n;

        Self {
            salt,
            private,
            public_b: pad(&public).to_vec(),
            session_key: None,
        }
    }

    /// Consumes the phone's M1/M3/M5 and returns what to send back.
    pub fn handle(&mut self, body: &[u8], pairings: &mut Pairings) -> Result<Vec<u8>, String> {
        let items = tlv_parse(body);
        let state = tlv_state(&items).ok_or("missing state")?;
        match state {
            1 => {
                tracing::info!("pairing.setup step=1 action=send_salt");
                Ok(tlv_encode(&vec![
                    (TYPE_STATE, vec![2]),
                    (TYPE_SALT, self.salt.to_vec()),
                    (TYPE_PUBLIC_KEY, self.public_b.clone()),
                ]))
            }
            3 => {
                let a_bytes = tlv_get(&items, TYPE_PUBLIC_KEY).ok_or("M3 without A")?;
                let proof = tlv_get(&items, TYPE_PROOF).ok_or("M3 without proof")?;
                let a = to_big(&a_bytes);
                let key = self.finish(&a);

                let n = n_prime();
                // HAP hashes the *unpadded* group bytes here: `g` is one byte
                // and `N` is however many it takes. Padding them (as `k` above
                // does) changes the proof and the phone rejects us.
                let hash_n = crypto::sha512(&n.to_bytes_be());
                let hash_g = crypto::sha512(&g().to_bytes_be());
                let mut xor = [0u8; 64];
                for i in 0..64 {
                    xor[i] = hash_n[i] ^ hash_g[i];
                }
                let expected = crypto::sha512_all(&[
                    &xor,
                    &crypto::sha512(USERNAME.as_bytes()),
                    &self.salt,
                    &pad(&a),
                    &pad(&self.public_b_biguint()),
                    &key,
                ]);
                if expected != proof[..] {
                    // Both sides are 64 bytes when the maths agrees; a length
                    // difference means the TLV was split, not that SRP failed.
                    tracing::error!(
                        "pairing.proof_mismatch got_bytes={} got={} want={}",
                        proof.len(),
                        hex::encode(&proof[..proof.len().min(8)]),
                        hex::encode(&expected[..8]),
                    );
                    return Err("controller proof mismatch".into());
                }
                let ours = crypto::sha512_all(&[&pad(&a), &expected, &key]);
                tracing::info!("pairing.setup step=3 result=srp_verified");
                Ok(tlv_encode(&vec![
                    (TYPE_STATE, vec![4]),
                    (TYPE_PROOF, ours.to_vec()),
                ]))
            }
            5 => self.finish_step5(&items, pairings),
            other => Err(format!("unexpected pair-setup state {other}")),
        }
    }

    /// Derives the SRP session key from the controller's public `A`.
    ///
    /// There is no failure path: every step is arithmetic on values we already
    /// validated, so this returns the key directly instead of a `Result`.
    fn finish(&mut self, a: &BigUint) -> [u8; 64] {
        let n = n_prime();
        let u = to_big(&crypto::sha512_all(&[
            &pad(a),
            &pad(&self.public_b_biguint()),
        ]));
        let s = (a * (self.verifier_ref().modpow(&u, &n))).modpow(&self.private, &n) % &n;
        // Unpadded, matching LIVI: `K = sha512(toBuf(S))`. `pad` is for the
        // group elements that go over the wire, not for hash inputs.
        let key = crypto::sha512(&s.to_bytes_be());
        self.session_key = Some(key);
        key
    }

    fn verifier_ref(&self) -> BigUint {
        let n = n_prime();
        let g = g();
        let x = to_big(&crypto::sha512_all(&[
            &self.salt,
            &crypto::sha512_all(&[USERNAME.as_bytes(), b":", PIN.as_bytes()]),
        ]));
        g.modpow(&x, &n)
    }

    fn public_b_biguint(&self) -> BigUint {
        to_big(&self.public_b)
    }

    /// M5 arrives sealed; inside is the controller's Ed25519 identity.
    fn finish_step5(&mut self, items: &Tlv, pairings: &mut Pairings) -> Result<Vec<u8>, String> {
        let key = self.session_key.ok_or("M5 before M3")?;
        let enc = tlv_get(items, TYPE_ENCRYPTED_DATA).ok_or("M5 without ciphertext")?;
        let sub_key = SymKey::from_bytes(&hkdf32(
            &key,
            "Pair-Setup-Encrypt-Salt",
            "Pair-Setup-Encrypt-Info",
        ))
        .map_err(|e| e.to_string())?;
        let plain = open(&sub_key, &nonce_label("PS-Msg05"), &enc, &[])
            .map_err(|_| String::from("could not open PS-Msg05"))?;
        let sub = tlv_parse(&plain);
        let ctrl_id = tlv_get(&sub, TYPE_IDENTIFIER).ok_or("M5 without identifier")?;
        let ctrl_ltpk: [u8; 32] = tlv_get(&sub, TYPE_PUBLIC_KEY)
            .and_then(|v| v.try_into().ok())
            .ok_or("M5 without public key")?;
        let ctrl_sig = tlv_get(&sub, TYPE_SIGNATURE).ok_or("M5 without signature")?;

        let sign_key = hkdf32(
            &key,
            "Pair-Setup-Controller-Sign-Salt",
            "Pair-Setup-Controller-Sign-Info",
        );
        let mut sign_data = Vec::new();
        sign_data.extend_from_slice(&sign_key);
        sign_data.extend_from_slice(&ctrl_id);
        sign_data.extend_from_slice(&ctrl_ltpk);
        ed25519_verify(&ctrl_ltpk, &sign_data, &ctrl_sig).map_err(|e| e.to_string())?;

        // M6: introduce ourselves the same way.
        let acc_key = hkdf32(
            &key,
            "Pair-Setup-Accessory-Sign-Salt",
            "Pair-Setup-Accessory-Sign-Info",
        );
        let acc_id = pairings.accessory_id.as_bytes().to_vec();
        let acc_ltpk = pairings.identity.public();
        let mut acc_data = Vec::new();
        acc_data.extend_from_slice(&acc_key);
        acc_data.extend_from_slice(&acc_id);
        acc_data.extend_from_slice(&acc_ltpk);
        let acc_sig = pairings.identity.sign(&acc_data).to_vec();

        let inner = tlv_encode(&vec![
            (TYPE_IDENTIFIER, acc_id),
            (TYPE_PUBLIC_KEY, acc_ltpk.to_vec()),
            (TYPE_SIGNATURE, acc_sig),
        ]);
        let sealed = seal(&sub_key, &nonce_label("PS-Msg06"), &inner, &[])
            .map_err(|_| String::from("could not seal PS-Msg06"))?;
        tracing::info!(
            "pairing.setup step=5 result=paired controller={:?}",
            String::from_utf8_lossy(&tlv_get(&sub, TYPE_IDENTIFIER).unwrap_or_default())
        );
        Ok(tlv_encode(&vec![
            (TYPE_STATE, vec![6]),
            (TYPE_ENCRYPTED_DATA, sealed),
        ]))
    }
}

// ------------------------------------------------------------ pair-verify ---

/// The root secret every later key derives from.
#[derive(Debug, Clone, Copy)]
pub struct VerifyKeys {
    pub shared: [u8; 32],
}

impl VerifyKeys {
    /// Control channel keys. Note the swap: what the phone writes, we read.
    pub fn control_read(&self) -> [u8; 32] {
        hkdf32(&self.shared, "Control-Salt", "Control-Write-Encryption-Key")
    }
    pub fn control_write(&self) -> [u8; 32] {
        hkdf32(&self.shared, "Control-Salt", "Control-Read-Encryption-Key")
    }
    /// The event channel does the opposite of the control channel: no swap.
    pub fn events_read(&self) -> [u8; 32] {
        hkdf32(&self.shared, "Events-Salt", "Events-Read-Encryption-Key")
    }
    pub fn events_write(&self) -> [u8; 32] {
        hkdf32(&self.shared, "Events-Salt", "Events-Write-Encryption-Key")
    }
    /// One media stream's decryption key, keyed by its stream id.
    pub fn media(&self, stream_id: u64) -> [u8; 32] {
        self.data_stream(stream_id, "DataStream-Output-Encryption-Key")
    }

    /// The mirror of the above, for the one stream that runs both ways.
    ///
    /// CarPlay carries the microphone on the phone's own audio stream rather
    /// than opening one of its own: the phone names a send port in that
    /// stream's SETUP, and what we put on it is sealed with this. Same salt and
    /// same key material as the downlink, a different label.
    pub fn media_input(&self, stream_id: u64) -> [u8; 32] {
        self.data_stream(stream_id, "DataStream-Input-Encryption-Key")
    }

    fn data_stream(&self, stream_id: u64, label: &str) -> [u8; 32] {
        hkdf_sha512(
            &self.shared,
            &format!("DataStream-Salt{stream_id}"),
            label,
            32,
        )
        .try_into()
        .expect("32 bytes")
    }
}

pub struct PairVerify {
    eph: Option<super::crypto::Ephemeral>,
    client_pub: Option<[u8; 32]>,
    /// Derived as soon as M1 lands, because M2 is sealed with it — but the
    /// *control channel* must not switch to encryption until M4 has gone out.
    shared: Option<VerifyKeys>,
    verified: bool,
}

impl Default for PairVerify {
    fn default() -> Self {
        Self::new()
    }
}

impl PairVerify {
    pub fn new() -> Self {
        Self {
            eph: None,
            client_pub: None,
            shared: None,
            verified: false,
        }
    }

    /// Only `Some` after M4 is answered. Handing these out any earlier makes the
    /// accessory encrypt its M2 reply, which the phone still expects in the clear.
    pub fn keys(&self) -> Option<VerifyKeys> {
        if self.verified {
            self.shared
        } else {
            None
        }
    }

    pub fn handle(&mut self, body: &[u8], pairings: &Pairings) -> Result<Vec<u8>, String> {
        let items = tlv_parse(body);
        let state = tlv_state(&items).ok_or("missing state")?;
        match state {
            1 => {
                tracing::info!("pairing.verify step=1 action=send_public_key");
                let client: [u8; 32] = tlv_get(&items, TYPE_PUBLIC_KEY)
                    .and_then(|v| v.try_into().ok())
                    .ok_or("M1 without public key")?;
                let eph = crypto::Ephemeral::generate();
                let shared = eph.shared(&client);
                self.client_pub = Some(client);

                let sub_key = SymKey::from_bytes(&hkdf32(
                    &shared,
                    "Pair-Verify-Encrypt-Salt",
                    "Pair-Verify-Encrypt-Info",
                ))
                .map_err(|e| e.to_string())?;

                let acc_id = pairings.accessory_id.as_bytes().to_vec();
                let mut sig_data = Vec::new();
                sig_data.extend_from_slice(&eph.public);
                sig_data.extend_from_slice(&acc_id);
                sig_data.extend_from_slice(&client);
                let sig = pairings.identity.sign(&sig_data).to_vec();

                let inner = tlv_encode(&vec![(TYPE_IDENTIFIER, acc_id), (TYPE_SIGNATURE, sig)]);
                let sealed = seal(&sub_key, &nonce_label("PV-Msg02"), &inner, &[])
                    .map_err(|_| String::from("could not seal PV-Msg02"))?;

                self.eph = Some(eph);
                self.shared = Some(VerifyKeys { shared });
                Ok(tlv_encode(&vec![
                    (TYPE_STATE, vec![2]),
                    (TYPE_PUBLIC_KEY, self.eph.as_ref().unwrap().public.to_vec()),
                    (TYPE_ENCRYPTED_DATA, sealed),
                ]))
            }
            3 => {
                tracing::info!("pairing.verify step=3 result=verified");
                let eph = self.eph.as_ref().ok_or("M3 before M1")?;
                let shared: [u8; 32] = self.shared.ok_or("no shared secret")?.shared;
                let _ = eph;
                let enc = tlv_get(&items, TYPE_ENCRYPTED_DATA).ok_or("M3 without ciphertext")?;
                let sub_key = SymKey::from_bytes(&hkdf32(
                    &shared,
                    "Pair-Verify-Encrypt-Salt",
                    "Pair-Verify-Encrypt-Info",
                ))
                .map_err(|e| e.to_string())?;
                let plain = open(&sub_key, &nonce_label("PV-Msg03"), &enc, &[])
                    .map_err(|_| String::from("could not open PV-Msg03"))?;
                let sub = tlv_parse(&plain);
                let ctrl_id = tlv_get(&sub, TYPE_IDENTIFIER).ok_or("M3 without identifier")?;
                let _ = ctrl_id;

                self.verified = true;
                Ok(tlv_encode(&vec![(TYPE_STATE, vec![4])]))
            }
            other => Err(format!("unexpected pair-verify state {other}")),
        }
    }
}
