//! The two halves of CarLife's authentication.
//!
//! The car satisfies itself that what is plugged into it is Baidu's own CarLife,
//! and the phone satisfies itself that the car is one it should serve. Each is
//! one message each way: a car whose challenge goes unanswered treats the device
//! as something else altogether, and a car whose channel the phone does not accept
//! is told so.
//!
//! The car's half is asymmetric cryptography. Baidu generates one key pair, keeps
//! the private half for its app and gives the public half to the head units built
//! to talk to it; a car sends a string and takes it back only if it has been
//! through that private key. Answering is therefore a matter of holding the key,
//! which is the app's own, extracted from the APK, because Baidu does not hand
//! that half out. It is a file the firmware installs in `crate::config::DATA_DIR`
//! and `Signer` puts the challenge through it. A machine that has none skips the
//! challenge instead: the head unit is left unauthenticated, asks again every half
//! minute, and the streams run either way.
//!
//! The phone's half has no secret in it: the car sends its channel number along
//! with its statistics, an app accepts it when it looks like one of the numbers
//! Baidu hands out, and the car is told the verdict — see `channel_is_legal`.
//!
//! Both are in the guide's §17. The channel rule is not: it was read out of the
//! app itself.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::Pkcs1v15Sign;
use rsa::pkcs8::DecodePrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;

/// The credential the phone's CarLife key is read from.
///
/// Not a credential of ours: it is the CarLife app's own private key, extracted
/// from the app, and the car accepts answers made with it because Baidu made the
/// matching public half. Like the MFi pair, the name is fixed rather than
/// configured: what it is called is not the operator's business, and it is a name
/// inside `crate::config::DATA_DIR` rather than a path.
pub(crate) const KEY_FILE: &str = "carlife_md_rsa_private.pem";

/// The CarLife version this device answers challenges as.
///
/// A challenge is `A_B`: a random string and the lowest phone version the car
/// will serve. The app compares `B` with its own version while it answers, and
/// answers as a phone that is too old when it is the older of the two — see
/// `answer_text`. What stands here is the version whose key the firmware
/// installs, so raising it is a matter of dropping in a newer key rather than of
/// changing this line.
const CLAIMED_VERSION: [u32; 3] = [8, 9, 0];

/// The key that answers the car's challenge.
///
/// `Clone` because there is one key and a session per car: `gadget` reads it
/// once and hands each session its own copy rather than parsing it again.
#[derive(Clone)]
pub struct Signer {
    key: RsaPrivateKey,
}

impl Signer {
    /// The key `crate::config::DATA_DIR` was given, when it was given one.
    ///
    /// The firmware installs it there beside the identity the program writes, so
    /// a board without one is a board whose build had no key to install. `None`
    /// is said out loud and leaves the CarLife side unable to answer a challenge:
    /// a supported way to run, so the line is a warning and the session goes on.
    pub fn load() -> Option<Self> {
        let path = crate::config::data_file(KEY_FILE);
        match std::fs::read_to_string(&path) {
            Ok(pem) => Self::from_pem(&pem),
            Err(e) => {
                tracing::warn!(
                    "auth.key_missing file={} err={e:#} consequence=carlife_auth_skipped",
                    path.display()
                );
                None
            }
        }
    }

    /// Reads the key out of `pem`, the text of `KEY_FILE` as it was installed.
    ///
    /// `None` is a key this cannot read, which is said out loud and leaves the
    /// CarLife side unable to answer a challenge.
    pub fn from_pem(pem: &str) -> Option<Self> {
        let Some(der) = pem_body(pem) else {
            tracing::warn!("auth.key_unreadable reason=no_pem");
            return None;
        };
        // Either shape of private key file will do: the PKCS#8 one a key
        // generator writes today, and the PKCS#1 one it is usually extracted
        // into. The label says which tool wrote the file, not what the key is.
        let key = RsaPrivateKey::from_pkcs8_der(&der)
            .or_else(|_| RsaPrivateKey::from_pkcs1_der(&der))
            .ok();
        let Some(key) = key else {
            tracing::warn!(
                "auth.key_unreadable reason=not_rsa consequence=challenge_unanswered"
            );
            return None;
        };
        tracing::info!("auth.key_loaded bits={}", key.size() * 8);
        Some(Self { key })
    }

    /// The answer to the car's challenge, in the shape the car is given it.
    ///
    /// The guide has the phone put the challenge through its private key and send
    /// the result; the app sends it Base64-encoded, which is what the car's
    /// `encryptValue` holds. Nothing is hashed in between: the whole of the
    /// challenge is the message, and the car recovers it with the public half and
    /// compares it with what it sent.
    ///
    /// `new_unprefixed` is the block a Java `RSA/ECB/PKCS1Padding` cipher puts
    /// out — `00 01 FF…FF 00` and then the text — which is the cipher the guide
    /// names and the one the app was found to use. That leaves the block's first
    /// byte a `01` rather than the `02` of an encryption padding, and a head unit
    /// has been found to accept it.
    pub fn answer(&self, challenge: &str) -> Option<String> {
        let text = answer_text(challenge);
        match self.key.sign(Pkcs1v15Sign::new_unprefixed(), text.as_bytes()) {
            Ok(answer) => Some(BASE64.encode(answer)),
            Err(e) => {
                tracing::warn!("auth.challenge_unanswered err={e}");
                None
            }
        }
    }
}

/// The text the car's challenge is answered with.
///
/// A phone whose own version is at least the one the challenge names answers with
/// the challenge as it stands. One that is older answers with the random part and
/// a version of zeros, which is how the app tells the car that it is too old for
/// it without a message of its own, and which the car reads as an answer that
/// does not match. The two versions are compared a segment at a time and the
/// first segment that differs decides.
///
/// Two challenges are answered with as they stand rather than with zeros: one
/// with no `_` in it, and one whose version is not three numbers. The app throws
/// on the first and sends nothing at all as a result; the second it reads as a
/// version it cannot compare. Neither is evidence that this device is too old,
/// and the car accepts the challenge back in both cases.
fn answer_text(challenge: &str) -> String {
    let Some((head, version)) = challenge.split_once('_') else {
        tracing::debug!("auth.challenge_version_missing action=answer_as_is");
        return challenge.to_string();
    };
    let Some(theirs) = version_of(version) else {
        tracing::debug!(
            "auth.challenge_version_unparsed version={version:?} action=answer_as_is"
        );
        return challenge.to_string();
    };
    for (theirs, ours) in theirs.iter().zip(CLAIMED_VERSION.iter()) {
        if theirs > ours {
            // Worth saying at a level that shows by default: a car that served
            // only newer versions is a car that will not accept the answer, and
            // this line is what says why.
            tracing::info!(
                "auth.car_version_too_new car={version} claimed={}.{}.{} action=answer_as_older",
                CLAIMED_VERSION[0],
                CLAIMED_VERSION[1],
                CLAIMED_VERSION[2]
            );
            return too_old(head);
        }
        if theirs < ours {
            break;
        }
    }
    challenge.to_string()
}

/// The random part with the version zeroed, which is what the app answers an
/// older phone with.
fn too_old(head: &str) -> String {
    format!("{head}_00.00.00")
}

fn version_of(text: &str) -> Option<[u32; 3]> {
    let mut parts = text.split('.');
    let (a, b, c) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    Some([a.parse().ok()?, b.parse().ok()?, c.parse().ok()?])
}

/// Whether a channel number is one a phone serves.
///
/// The app checks three things and nothing else: eight decimal digits with a
/// leading 2, and not either of the two numbers that stand for "no particular
/// car" — `20000000` for the aftermarket and `20000001` for a pre-installed app,
/// which its own table marks as invalid. There is no list of approved numbers
/// anywhere, which is what makes this cheap to satisfy: any other `2xxxxxxx` is
/// accepted, whether or not it appears in the table of 64 the app ships with.
///
/// One channel needs more than this and gets it anyway. `20062100` (Daimler) is
/// the only one whose app also compares its own package signature with Baidu's,
/// which is a check against a repackaged app; this device is not that app, so
/// there is nothing here for it to fail.
pub fn channel_is_legal(channel: &str) -> bool {
    let channel = channel.trim();
    channel.len() == 8
        && channel.starts_with('2')
        && channel.bytes().all(|b| b.is_ascii_digit())
        && channel != "20000000"
        && channel != "20000001"
}

/// The DER inside a PEM block, whatever the block calls itself.
///
/// Both `-----BEGIN PRIVATE KEY-----` and `-----BEGIN RSA PRIVATE KEY-----` are
/// RSA private keys written by different tools; the label is the only thing that
/// tells them apart and the only thing this ignores.
fn pem_body(pem: &str) -> Option<Vec<u8>> {
    let start = pem.find("-----BEGIN")?;
    let body_start = start + pem[start..].find('\n')? + 1;
    let body_end = body_start + pem[body_start..].find("-----END")?;
    let body: Vec<u8> = pem[body_start..body_end]
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    BASE64.decode(body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::BigUint;
    use rsa::RsaPublicKey;

    /// A key for the tests to work with, made here rather than dropped into the
    /// repository: what they test is the road a challenge travels, not any
    /// particular key. 1024 bits is the size the real one is.
    fn a_key() -> RsaPrivateKey {
        RsaPrivateKey::new(&mut rand::thread_rng(), 1024).expect("a key can be generated")
    }

    /// What the car does with an answer: the public half of the key, applied to
    /// the block. Done here with the primitive rather than with the same crate
    /// call that produced it, so the test can look at the block itself.
    fn recovered(key: &RsaPrivateKey, sealed: &[u8]) -> Vec<u8> {
        let public = key.to_public_key();
        let block = BigUint::from_bytes_be(sealed).modpow(public.e(), public.n());
        let bytes = block.to_bytes_be();
        let mut out = vec![0u8; public.size()];
        out[public.size() - bytes.len()..].copy_from_slice(&bytes);
        out
    }

    #[test]
    fn the_challenge_comes_back_out_of_the_answer_the_car_is_given() {
        let key = a_key();
        let signer = Signer { key: key.clone() };
        let challenge = "9f2c1a_4.6.0";

        let answer = signer.answer(challenge).expect("the challenge is answered");
        let sealed = BASE64.decode(&answer).expect("the answer is Base64");
        assert_eq!(sealed.len(), 128, "one block of a 1024-bit key");

        // What a PKCS#1 v1.5 block is: a null, a type, a run of padding, a null,
        // and then the message at the end of the block.
        let block = recovered(&key, &sealed);
        assert_eq!(&block[..2], [0x00, 0x01], "the type the app's cipher writes");
        let padding = block[2..].iter().take_while(|b| **b == 0xff).count();
        assert!(padding >= 8, "at least eight bytes of padding, got {padding}");
        assert_eq!(block[2 + padding], 0x00);
        assert_eq!(
            &block[3 + padding..],
            challenge.as_bytes(),
            "the challenge sits at the end of the block"
        );

        // And what the car's own check amounts to.
        assert!(key
            .to_public_key()
            .verify(
                Pkcs1v15Sign::new_unprefixed(),
                challenge.as_bytes(),
                &sealed
            )
            .is_ok());
    }

    #[test]
    fn a_car_that_serves_only_newer_versions_is_answered_as_an_older_phone() {
        for challenge in ["abcd_1.0.0", "abcd_4.6.0", "abcd_8.9.0", "abcd_7.9.9"] {
            assert_eq!(answer_text(challenge), challenge, "{challenge} stands");
        }
        for challenge in ["abcd_9.0.0", "abcd_8.9.1", "abcd_8.10.0", "abcd_10.0.0"] {
            let head = challenge.split('_').next().unwrap();
            assert_eq!(
                answer_text(challenge),
                format!("{head}_00.00.00"),
                "{challenge} is too new for this device"
            );
        }
    }

    #[test]
    fn a_challenge_with_no_version_to_compare_is_answered_as_it_stands() {
        // The app throws on the first of these and sends nothing as a result;
        // the second and third it cannot read as a version at all.
        for challenge in ["abcdef", "abcd_1.0", "abcd_1.0.0.0", "abcd_x.y.z"] {
            assert_eq!(answer_text(challenge), challenge);
        }
    }

    #[test]
    fn a_channel_number_is_judged_by_its_shape_and_two_numbers_that_are_not_cars() {
        for legal in ["20011100", "20062100", "20152100", "21350001", " 20011100 "] {
            assert!(channel_is_legal(legal), "{legal} is served");
        }
        for illegal in [
            "",
            " ",
            "20000000",
            "20000001",
            "10011100",
            "2001110",
            "200111000",
            "2001110a",
            "2#011100",
            "２0011100",
        ] {
            assert!(!channel_is_legal(illegal), "{illegal:?} is not served");
        }
    }

    #[test]
    fn a_key_file_is_read_in_either_of_the_two_shapes_it_comes_in() {
        let key = a_key();
        let pkcs8 = key.to_pkcs8_der().expect("pkcs8").as_bytes().to_vec();
        let pkcs1 = key.to_pkcs1_der().expect("pkcs1").as_bytes().to_vec();

        let pem = |der: &[u8], label: &str| {
            format!(
                "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
                BASE64.encode(der)
            )
        };
        assert_eq!(pem_body(&pem(&pkcs8, "PRIVATE KEY")), Some(pkcs8));
        assert_eq!(pem_body(&pem(&pkcs1, "RSA PRIVATE KEY")), Some(pkcs1));

        // Anything that is not a PEM block is not one, rather than a panic.
        assert_eq!(pem_body("no key here"), None);
        assert_eq!(pem_body("-----BEGIN PRIVATE KEY-----\nnot base64!\n-----END"), None);
    }

    /// The two halves the car will be answered with and checks with, when this
    /// machine has them: this is where the protocol leaves the fixtures behind and
    /// meets the material under `authentication/`, including whether the file
    /// extracted from the app and the public half the report printed are really
    /// one pair.
    ///
    /// A machine without them asserts nothing, because running without a key is a
    /// supported way to run.
    #[test]
    fn the_key_dropped_in_for_a_car_is_one_the_car_accepts() {
        use rsa::pkcs8::DecodePublicKey;

        let dir = std::path::Path::new("authentication");
        let private = dir.join("carlife_md_rsa_private.pem");
        let public = dir.join("carlife_md_rsa_public.pem");
        if !private.is_file() || !public.is_file() {
            return;
        }

        let pem = std::fs::read_to_string(&private).expect("the key is readable");
        let signer = Signer::from_pem(&pem).expect("a key this can read");
        // The challenge the extraction report verified its own round trip with.
        let challenge = "1234567890_1.0.0";
        let answer = signer.answer(challenge).expect("the challenge is answered");
        let sealed = BASE64.decode(answer).expect("the answer is Base64");

        let key = RsaPublicKey::read_public_key_pem_file(&public).expect("a public key this reads");
        key.verify(
            Pkcs1v15Sign::new_unprefixed(),
            challenge.as_bytes(),
            &sealed,
        )
        .expect("the half the car holds accepts the answer");
    }
}
