//! Root certificates the operating system trusts for the current user.

use std::fmt::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Base64 characters per PEM line.
const PEM_LINE: usize = 64;

/// DER encodings of the root certificates Windows trusts for the current user, which include those of the machine
/// and those an organization deploys through Group Policy.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn system_roots() -> std::io::Result<Vec<Vec<u8>>> {
    use windows_sys::Win32::Security::Cryptography::{
        CERT_CONTEXT, CertCloseStore, CertEnumCertificatesInStore, CertOpenSystemStoreW,
    };
    use windows_sys::w;

    // SAFETY: the store name is a NUL-terminated literal; no legacy provider is passed.
    let store = unsafe { CertOpenSystemStoreW(0, w!("ROOT")) };
    if store.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let mut roots = Vec::new();
    let mut context: *const CERT_CONTEXT = std::ptr::null();
    loop {
        // SAFETY: `store` is open; `context` is null or the previous result, which this call releases.
        context = unsafe { CertEnumCertificatesInStore(store, context) };
        if context.is_null() {
            break;
        }
        // SAFETY: a returned context is valid until the next call, and its encoding holds `cbCertEncoded` bytes.
        let der = unsafe { std::slice::from_raw_parts((*context).pbCertEncoded, (*context).cbCertEncoded as usize) };
        roots.push(der.to_vec());
    }
    // SAFETY: `store` is open, the enumeration released its last context, and the store is closed once.
    unsafe { CertCloseStore(store, 0) };

    Ok(roots)
}

/// `certificates`, each DER-encoded, as one PEM bundle.
pub fn pem(certificates: &[Vec<u8>]) -> String {
    certificates.iter().fold(String::new(), |mut bundle, der| {
        let encoded = STANDARD.encode(der);
        bundle.push_str("-----BEGIN CERTIFICATE-----\n");
        for line in encoded.as_bytes().chunks(PEM_LINE) {
            let _ = writeln!(bundle, "{}", String::from_utf8_lossy(line));
        }
        bundle.push_str("-----END CERTIFICATE-----\n");
        bundle
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_each_certificate_at_64_characters() {
        let bundle = pem(&[vec![0xAB; 60], vec![1, 2, 3]]);

        let lines: Vec<&str> = bundle.lines().collect();
        assert_eq!(lines[0], "-----BEGIN CERTIFICATE-----");
        assert_eq!(lines[1].len(), 64);
        assert_eq!(lines[2].len(), 16);
        assert_eq!(lines[3], "-----END CERTIFICATE-----");
        assert_eq!(
            &lines[4..],
            ["-----BEGIN CERTIFICATE-----", "AQID", "-----END CERTIFICATE-----"]
        );
        assert!(pem(&[]).is_empty());
    }
}
