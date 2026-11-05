//! A private CA and server certificate, signed with Graviola so that no C crypto gets linked for a test tool.

use std::sync::Arc;

use graviola::hashing::Sha256;
use graviola::signing::ecdsa::{SigningKey, P256};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyUsagePurpose, PublicKeyData, SerialNumber, SignatureAlgorithm};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

struct Key {
    key: SigningKey<P256>,
    public: Vec<u8>,
}

impl Key {
    fn generate() -> Result<Self, String> {
        let key = SigningKey::<P256>::generate().map_err(|e| format!("keygen: {e:?}"))?;
        let mut buf = [0u8; 128];
        let spki = key.to_spki_der(&mut buf).map_err(|e| format!("spki: {e:?}"))?;
        // The uncompressed point is the last 65 bytes of a P-256 SubjectPublicKeyInfo.
        let public = spki[spki.len() - 65..].to_vec();
        Ok(Self { key, public })
    }

    fn pkcs8(&self) -> Result<Vec<u8>, String> {
        let mut buf = [0u8; 256];
        Ok(self.key.to_pkcs8_der(&mut buf).map_err(|e| format!("pkcs8: {e:?}"))?.to_vec())
    }
}

impl PublicKeyData for Key {
    fn der_bytes(&self) -> &[u8] {
        &self.public
    }
    fn algorithm(&self) -> &'static SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl rcgen::SigningKey for Key {
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        let mut out = [0u8; 128];
        let sig = self.key.sign_asn1::<Sha256>(&[msg], &mut out).map_err(|_| rcgen::Error::RemoteKeyError)?;
        Ok(sig.to_vec())
    }
}

/// Positive 16-byte serial, unique enough that two runs' certificates are never confused.
fn serial() -> SerialNumber {
    let mut bytes = [0u8; 16];
    fastrand::fill(&mut bytes);
    bytes[0] = (bytes[0] & 0x7f) | 0x40;
    SerialNumber::from_slice(&bytes)
}

pub struct Identity {
    pub ca_pem: String,
    pub cert_der: Vec<u8>,
    pub key_pkcs8: Vec<u8>,
}

/// Issues a new CA and a certificate for `hosts` (DNS names or IP addresses).
pub fn generate(hosts: &[String]) -> Result<Identity, String> {
    let ca_key = Key::generate()?;
    let mut ca = CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    ca.serial_number = Some(serial());
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca.distinguished_name.push(DnType::CommonName, "chaosgw test CA");
    let ca_cert = ca.self_signed(&ca_key).map_err(|e| e.to_string())?;

    let leaf_key = Key::generate()?;
    let mut leaf = CertificateParams::new(hosts.to_vec()).map_err(|e| e.to_string())?;
    leaf.serial_number = Some(serial());
    leaf.distinguished_name.push(DnType::CommonName, "chaosgw");
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let issuer = Issuer::from_params(&ca, &ca_key);
    let leaf_cert = leaf.signed_by(&leaf_key, &issuer).map_err(|e| e.to_string())?;

    Ok(Identity { ca_pem: ca_cert.pem(), cert_der: leaf_cert.der().to_vec(), key_pkcs8: leaf_key.pkcs8()? })
}

/// Server side of TLS, offering h2 and HTTP/1.1.
pub fn server_config(id: &Identity) -> Result<Arc<rustls::ServerConfig>, String> {
    let chain = vec![CertificateDer::from(id.cert_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.key_pkcs8.clone()));
    let mut config = rustls::ServerConfig::builder_with_provider(rustls_graviola::default_provider().into())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| e.to_string())?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}
