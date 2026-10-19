//! Exact appliance enrollment for Hue's legacy certificates without DNS SANs.
//! This trust mode authenticates the enrolled leaf rather than a DNS PKI name.
use anyhow::{Context, Result, ensure};
use rustls::{
    ClientConfig, DigitallySignedStruct, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use std::{io::Read, path::Path, sync::Arc};
use x509_parser::{
    prelude::{FromDer, X509Certificate},
    time::ASN1Time,
};

pub(super) fn pinned_config(path: &Path, bridge_id: &str) -> Result<ClientConfig> {
    ensure!(
        bridge_id.len() == 16 && bridge_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid enrolled bridge ID"
    );
    let mut pem = Vec::new();
    std::fs::File::open(path)?
        .take(65537)
        .read_to_end(&mut pem)?;
    ensure!(pem.len() <= 65536, "enrolled certificate too large");
    let certificates =
        rustls_pemfile::certs(&mut pem.as_slice()).collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        certificates.len() == 1,
        "enroll exactly one bridge leaf certificate"
    );
    let certificate = certificates
        .into_iter()
        .next()
        .context("missing certificate")?;
    identity_and_time(&certificate, bridge_id, UnixTime::now())?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(EnrolledBridge {
        certificate,
        bridge_id: bridge_id.to_owned(),
        provider: provider.clone(),
    });
    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

fn identity_and_time(
    certificate: &CertificateDer<'_>,
    bridge_id: &str,
    now: UnixTime,
) -> Result<(), Error> {
    let (remaining, parsed) = X509Certificate::from_der(certificate.as_ref())
        .map_err(|_| Error::General("invalid enrolled Hue certificate".into()))?;
    if !remaining.is_empty() {
        return Err(Error::General("trailing certificate data".into()));
    }
    let mut names = parsed.subject().iter_common_name();
    let name = names.next().and_then(|name| name.as_str().ok());
    if !name.is_some_and(|name| name.eq_ignore_ascii_case(bridge_id)) || names.next().is_some() {
        return Err(Error::General(
            "certificate does not match enrolled Hue bridge ID".into(),
        ));
    }
    let now = i64::try_from(now.as_secs())
        .ok()
        .and_then(|now| ASN1Time::from_timestamp(now).ok())
        .ok_or_else(|| Error::General("invalid certificate verification time".into()))?;
    if !parsed.validity().is_valid_at(now) {
        return Err(Error::General(
            "enrolled Hue certificate is not currently valid".into(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct EnrolledBridge {
    certificate: CertificateDer<'static>,
    bridge_id: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for EnrolledBridge {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if end_entity != &self.certificate {
            return Err(Error::General(
                "Hue peer differs from the enrolled certificate".into(),
            ));
        }
        identity_and_time(end_entity, &self.bridge_id, now)?;
        // The subsequent signature verification proves possession of the
        // private key belonging to this exact, enrolled certificate.
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
