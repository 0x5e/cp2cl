// TLS 1.2 in frames, from the phone's side: the head unit is the client, we are
// the server, and one session carries every channel.
//
// This is LIVI's `livi-aa` TLS engine with the direction turned around. The
// shape is the same — handshake bytes ride in plaintext SSL_HANDSHAKE frames,
// afterwards one frame payload is one TLS record — but we accept instead of
// connect, and the certificate we present is the phone's.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{ring, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{
    DigitallySignedStruct, DistinguishedName, ServerConfig, ServerConnection, SignatureScheme,
};

use super::consts::{FLAG_FIRST, FLAG_LAST};
use super::frame;

/// Cleartext goes into TLS in slices this size, so one frame stays under the
/// 2 kB that head units are happiest with over USB. rustls turns each slice
/// into a record, `frame` turns each record into a frame.
const MAX_CLEARTEXT_CHUNK: usize = 1800;

/// One decrypted AA message.
#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub ch: u8,
    pub flags: u8,
    pub msg_id: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub enum Error {
    Tls(rustls::Error),
    Io(std::io::Error),
    Cert(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Tls(e) => write!(f, "tls: {e}"),
            Error::Io(e) => write!(f, "tls io: {e}"),
            Error::Cert(e) => write!(f, "tls cert: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Accepts whatever certificate the head unit offers.
///
/// Not laziness: Android Auto handshakes with certificates that are expired and
/// self-signed on purpose, so there is nothing for a chain check to find, and a
/// verifier that insists would only refuse real hardware. The head unit checks
/// us just as little — see `verifyCertificate` in AACS's `AaCommunicator`.
#[derive(Debug)]
struct AcceptAny(WebPkiSupportedAlgorithms);

impl ClientCertVerifier for AcceptAny {
    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    /// Offered, not demanded: head units that present nothing still work.
    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

/// The phone's certificate and key, as read from PEM.
pub struct PhoneIdentity {
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl PhoneIdentity {
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, Error> {
        // Both readers advance through the slice, so they need their own cursor.
        let mut cert_cursor = cert_pem;
        let mut key_cursor = key_pem;
        let certs = rustls_pemfile::certs(&mut cert_cursor)
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::Io)?;
        if certs.is_empty() {
            return Err(Error::Cert("no certificate in PEM".into()));
        }
        let key = rustls_pemfile::private_key(&mut key_cursor)
            .map_err(Error::Io)?
            .ok_or_else(|| Error::Cert("no private key in PEM".into()))?;
        Ok(Self { certs, key })
    }

    /// `android_auto.crt` + `android_auto.key`, the names AACS ships them under.
    pub fn from_dir(dir: &Path) -> Result<Self, Error> {
        let cert = std::fs::read(dir.join("android_auto.crt"))
            .map_err(|e| Error::Cert(format!("{}: {e}", dir.join("android_auto.crt").display())))?;
        let key = std::fs::read(dir.join("android_auto.key"))
            .map_err(|e| Error::Cert(format!("{}: {e}", dir.join("android_auto.key").display())))?;
        Self::from_pem(&cert, &key)
    }

    /// The first directory that has both files, in the order a developer would
    /// look: an override, next to the binary, next to the MFi credentials, then
    /// the AACS checkout these came from.
    pub fn load_default() -> Result<Self, Error> {
        let mut candidates: Vec<PathBuf> = match std::env::var_os("CP2CL_AA_CERT_DIR") {
            Some(dir) => vec![PathBuf::from(dir)],
            None => Vec::new(),
        };
        candidates.extend([
            PathBuf::from("aa"),
            PathBuf::from("authentication"),
            PathBuf::from("../AACS/AAServer/ssl"),
        ]);
        let mut tried = Vec::new();
        for dir in candidates {
            match Self::from_dir(&dir) {
                Ok(identity) => {
                    tracing::info!("[aa] phone certificate from {}", dir.display());
                    return Ok(identity);
                }
                Err(e) => tried.push(format!("{} ({e})", dir.display())),
            }
        }
        Err(Error::Cert(format!(
            "no phone certificate found; tried {}",
            tried.join(", ")
        )))
    }

    /// Built directly rather than through a config builder: Android Auto's
    /// certificates are v1, which webpki refuses to parse.
    pub fn certified(&self, provider: &CryptoProvider) -> Result<Arc<CertifiedKey>, Error> {
        let signing = provider
            .key_provider
            .load_private_key(self.key.clone_key())
            .map_err(Error::Tls)?;
        Ok(Arc::new(CertifiedKey::new(self.certs.clone(), signing)))
    }
}

/// Presents the phone's certificate to whoever asks.
#[derive(Debug)]
struct PhoneCert(Arc<CertifiedKey>);

impl ResolvesServerCert for PhoneCert {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

pub fn server_config(identity: &PhoneIdentity) -> Result<Arc<ServerConfig>, Error> {
    let provider = Arc::new(ring::default_provider());
    let algorithms = provider.signature_verification_algorithms;
    // Resolved directly rather than through `with_single_cert`, which would run
    // the v1 certificate past webpki.
    let key = identity.certified(&provider)?;
    // A server sets its client verifier directly — only the client side hides
    // that behind `dangerous()`, which is why there is no such call here.
    let config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12])
        .map_err(Error::Tls)?
        .with_client_cert_verifier(Arc::new(AcceptAny(algorithms)))
        .with_cert_resolver(Arc::new(PhoneCert(key)));
    Ok(Arc::new(config))
}

pub struct TlsEngine {
    conn: ServerConnection,
    /// Cleartext fragments per channel, with the flags of the first one.
    fragments: HashMap<u8, (u8, Vec<u8>)>,
}

impl TlsEngine {
    pub fn new(identity: &PhoneIdentity) -> Result<Self, Error> {
        let conn = ServerConnection::new(server_config(identity)?).map_err(Error::Tls)?;
        Ok(Self {
            conn,
            fragments: HashMap::new(),
        })
    }

    pub fn is_handshaking(&self) -> bool {
        self.conn.is_handshaking()
    }

    /// The TLS bytes waiting to go to the head unit.
    pub fn take_output(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while self.conn.wants_write() {
            if self.conn.write_tls(&mut out).is_err() {
                break;
            }
        }
        out
    }

    /// Handshake bytes out of an SSL_HANDSHAKE frame.
    pub fn inject_handshake(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.feed(bytes).map(|_| ())
    }

    /// One encrypted frame payload, and the messages that completed with it.
    pub fn inject_record(
        &mut self,
        ch: u8,
        flags: u8,
        record: &[u8],
    ) -> Result<Vec<Message>, Error> {
        let plain = self.feed(record)?;
        let mut out = Vec::new();
        if plain.is_empty() {
            return Ok(out);
        }
        let first = flags & FLAG_FIRST != 0;
        let last = flags & FLAG_LAST != 0;
        if first && last {
            out.extend(message(ch, flags, plain));
            return Ok(out);
        }
        if first {
            self.fragments.insert(ch, (flags, plain));
            return Ok(out);
        }
        let Some((first_flags, mut buf)) = self.fragments.remove(&ch) else {
            return Ok(out);
        };
        buf.extend_from_slice(&plain);
        if !last {
            self.fragments.insert(ch, (first_flags, buf));
            return Ok(out);
        }
        out.extend(message(ch, first_flags, buf));
        Ok(out)
    }

    /// Encrypts one message into wire frames, splitting the cleartext so no
    /// single frame grows past what a head unit will take.
    pub fn encrypt(
        &mut self,
        ch: u8,
        flags: u8,
        msg_id: u16,
        data: &[u8],
    ) -> Result<Vec<u8>, Error> {
        let mut clear = Vec::with_capacity(2 + data.len());
        clear.extend_from_slice(&msg_id.to_be_bytes());
        clear.extend_from_slice(data);
        for chunk in clear.chunks(MAX_CLEARTEXT_CHUNK) {
            self.conn.writer().write_all(chunk).map_err(Error::Io)?;
        }
        let tls = self.take_output();
        Ok(frame::encode_records(ch, flags, &tls))
    }

    fn feed(&mut self, mut bytes: &[u8]) -> Result<Vec<u8>, Error> {
        while !bytes.is_empty() {
            let n = self.conn.read_tls(&mut bytes).map_err(Error::Io)?;
            self.conn.process_new_packets().map_err(Error::Tls)?;
            if n == 0 {
                return Err(Error::Io(std::io::Error::other("tls input stalled")));
            }
        }
        let mut plain = Vec::new();
        match self.conn.reader().read_to_end(&mut plain) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(Error::Io(e)),
        }
        Ok(plain)
    }
}

fn message(ch: u8, flags: u8, buf: Vec<u8>) -> Option<Message> {
    if buf.len() < 2 {
        return None;
    }
    Some(Message {
        ch,
        flags,
        msg_id: u16::from_be_bytes([buf[0], buf[1]]),
        payload: buf[2..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aa_client::consts::FLAGS_ENC_SIGNAL;
    use crate::aa_client::frame::FrameSplitter;
    use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, ClientConnection};

    /// A head-unit stand-in: takes our certificate without looking at it.
    #[derive(Debug)]
    struct AcceptServerCert(WebPkiSupportedAlgorithms);

    impl ServerCertVerifier for AcceptServerCert {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.supported_schemes()
        }
    }

    fn hu() -> ClientConnection {
        let provider = Arc::new(ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptServerCert(
                provider.signature_verification_algorithms,
            )))
            .with_no_client_auth();
        ClientConnection::new(Arc::new(config), ServerName::try_from("headunit").unwrap()).unwrap()
    }

    fn pump(hu: &mut ClientConnection, bytes: &[u8]) -> Vec<u8> {
        let mut input = bytes;
        while !input.is_empty() {
            hu.read_tls(&mut input).unwrap();
            hu.process_new_packets().unwrap();
        }
        let mut out = Vec::new();
        while hu.wants_write() {
            hu.write_tls(&mut out).unwrap();
        }
        out
    }

    fn handshake() -> (TlsEngine, ClientConnection) {
        // Whatever phone certificate is lying around: TLS is what these tests
        // are about, not the credentials.
        let identity = PhoneIdentity::load_default().expect(
            "no phone certificate found — put android_auto.crt + android_auto.key in ./aa, or \
             set CP2CL_AA_CERT_DIR",
        );
        let mut phone = TlsEngine::new(&identity).unwrap();
        let mut hu = hu();
        // The head unit speaks first, which is the whole point of being a server.
        let to_phone = pump(&mut hu, &[]);
        phone.inject_handshake(&to_phone).unwrap();
        for _ in 0..8 {
            let to_hu = phone.take_output();
            let to_phone = pump(&mut hu, &to_hu);
            if !to_phone.is_empty() {
                phone.inject_handshake(&to_phone).unwrap();
            }
            if !phone.is_handshaking() && !hu.is_handshaking() {
                break;
            }
        }
        assert!(!phone.is_handshaking(), "phone still handshaking");
        assert!(!hu.is_handshaking(), "head unit still handshaking");
        (phone, hu)
    }

    #[test]
    fn the_handshake_completes_over_frames() {
        handshake();
    }

    #[test]
    fn a_message_from_the_head_unit_is_decrypted_with_its_channel() {
        let (mut phone, mut hu) = handshake();
        hu.writer().write_all(&[0x80, 0x01, 0x08, 0x07]).unwrap();
        let records = pump(&mut hu, &[]);
        let wire = frame::encode_records(3, FLAGS_ENC_SIGNAL, &records);

        let mut s = FrameSplitter::default();
        s.push(&wire);
        let f = s.next_frame().unwrap();
        let msgs = phone.inject_record(f.ch, f.flags, &f.payload).unwrap();
        assert_eq!(
            msgs,
            vec![Message {
                ch: 3,
                flags: FLAGS_ENC_SIGNAL,
                msg_id: 0x8001,
                payload: vec![8, 7]
            }]
        );
    }

    #[test]
    fn a_message_to_the_head_unit_arrives_in_clear() {
        let (mut phone, mut hu) = handshake();
        let wire = phone
            .encrypt(4, FLAGS_ENC_SIGNAL, 0x8004, &[8, 1, 16, 1])
            .unwrap();
        let mut s = FrameSplitter::default();
        s.push(&wire);
        let f = s.next_frame().unwrap();
        assert_eq!((f.ch, f.flags), (4, FLAGS_ENC_SIGNAL));
        pump(&mut hu, &f.payload);
        let mut clear = Vec::new();
        hu.reader().read_to_end(&mut clear).ok();
        assert_eq!(clear, vec![0x80, 0x04, 8, 1, 16, 1]);
    }

    #[test]
    fn a_message_bigger_than_one_frame_stays_one_message() {
        let (mut phone, mut hu) = handshake();
        let big = vec![0x5A; 40_000];
        let wire = phone.encrypt(3, FLAGS_ENC_SIGNAL, 0x0000, &big).unwrap();

        let mut s = FrameSplitter::default();
        s.push(&wire);
        let mut frames = 0;
        let mut clear = Vec::new();
        while let Some(f) = s.next_frame() {
            frames += 1;
            assert!(
                f.payload.len() <= MAX_CLEARTEXT_CHUNK + 64,
                "frame of {}B",
                f.payload.len()
            );
            // TLS is directional: records we encrypted can only be read by the
            // head unit, which is what `hu` stands in for here.
            pump(&mut hu, &f.payload);
            hu.reader().read_to_end(&mut clear).ok();
        }
        assert!(frames > 1, "a 40 kB message should need several frames");
        // What the head unit reads is the message id we wrote, then the bytes.
        let mut expected = 0x0000u16.to_be_bytes().to_vec();
        expected.extend_from_slice(&big);
        assert_eq!(clear, expected);
    }
}
