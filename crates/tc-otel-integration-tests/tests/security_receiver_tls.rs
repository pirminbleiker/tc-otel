//! Security tests for OTEL receiver - TLS configuration validation
//!
//! These tests verify that the TLS security configuration correctly
//! rejects insecure settings and enforces security policies.

use std::path::PathBuf;
use tc_otel_core::{ReceiverConfig, TlsConfig};

/// Helper: create a valid TLS config with cert paths set
fn valid_tls_config() -> TlsConfig {
    TlsConfig {
        enabled: true,
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ca_cert_path: Some(PathBuf::from("/etc/ssl/certs/ca-bundle.crt")),
        ..TlsConfig::default()
    }
}

#[test]
fn test_security_tls_https_only_enforcement() {
    // SECURITY: When https_only=true, HTTP endpoints must be rejected.
    // Only HTTPS endpoints are accepted.
    let config = ReceiverConfig {
        https_only: true,
        tls: valid_tls_config(),
        ..ReceiverConfig::default()
    };

    // HTTP endpoint must be rejected
    let result = config.validate_endpoint("http://localhost:4318/v1/logs");
    assert!(
        result.is_err(),
        "HTTP endpoint must be rejected when https_only=true"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("https_only"),
        "Error should mention https_only: {err}"
    );

    // HTTPS endpoint must be accepted
    let result = config.validate_endpoint("https://localhost:4318/v1/logs");
    assert!(
        result.is_ok(),
        "HTTPS endpoint must be accepted when https_only=true"
    );

    // Overall config must be valid (https_only with TLS enabled)
    assert!(
        config.validate().is_ok(),
        "https_only=true with TLS enabled should be valid"
    );

    // https_only without TLS enabled is invalid
    let bad_config = ReceiverConfig {
        https_only: true,
        tls: TlsConfig::default(), // enabled=false
        ..ReceiverConfig::default()
    };
    let result = bad_config.validate();
    assert!(
        result.is_err(),
        "https_only=true without TLS enabled must be rejected"
    );
}

#[test]
fn test_security_tls_certificate_validation() {
    // SECURITY: TLS certificates must be validated — insecure_skip_verify
    // is never allowed. Missing cert paths must be caught.

    // insecure_skip_verify=true must be rejected
    let insecure = TlsConfig {
        enabled: true,
        insecure_skip_verify: true,
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ..TlsConfig::default()
    };
    let result = insecure.validate();
    assert!(
        result.is_err(),
        "insecure_skip_verify=true must be rejected"
    );
    let errors = result.unwrap_err();
    assert!(
        errors.iter().any(|e| e.contains("insecure_skip_verify")),
        "Error should mention insecure_skip_verify: {errors:?}"
    );

    // Missing cert_path must be rejected
    let missing_cert = TlsConfig {
        enabled: true,
        cert_path: None,
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ..TlsConfig::default()
    };
    let result = missing_cert.validate();
    assert!(result.is_err(), "Missing cert_path must be rejected");
    let errors = result.unwrap_err();
    assert!(
        errors.iter().any(|e| e.contains("cert_path")),
        "Error should mention cert_path: {errors:?}"
    );

    // Missing key_path must be rejected
    let missing_key = TlsConfig {
        enabled: true,
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: None,
        ..TlsConfig::default()
    };
    let result = missing_key.validate();
    assert!(result.is_err(), "Missing key_path must be rejected");

    // Valid config must pass
    let valid = valid_tls_config();
    assert!(
        valid.validate().is_ok(),
        "Valid TLS config must pass validation"
    );
}

#[test]
fn test_security_tls_minimum_version_enforcement() {
    // SECURITY: Only TLS 1.2+ must be accepted as minimum version.
    // TLS 1.0 and 1.1 are insecure and must be rejected.

    // TLS 1.0 as min_version must be rejected
    let tls10 = TlsConfig {
        enabled: true,
        min_version: "TLSv1_0".to_string(),
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ..TlsConfig::default()
    };
    let result = tls10.validate();
    assert!(result.is_err(), "TLS 1.0 must be rejected");
    let errors = result.unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("TLSv1_0") && e.contains("below minimum")),
        "Error should indicate TLSv1_0 is below minimum: {errors:?}"
    );

    // TLS 1.1 must also be rejected
    let tls11 = TlsConfig {
        enabled: true,
        min_version: "TLSv1_1".to_string(),
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ..TlsConfig::default()
    };
    let result = tls11.validate();
    assert!(result.is_err(), "TLS 1.1 must be rejected");

    // TLS 1.2 must be accepted
    assert!(
        TlsConfig::check_minimum_acceptable_version("TLSv1_2").is_ok(),
        "TLS 1.2 must be accepted"
    );

    // TLS 1.3 must be accepted
    assert!(
        TlsConfig::check_minimum_acceptable_version("TLSv1_3").is_ok(),
        "TLS 1.3 must be accepted"
    );

    // Invalid version string must be caught
    assert!(
        TlsConfig::validate_tls_version("SSLv3").is_err(),
        "SSLv3 must be rejected as invalid TLS version"
    );
}

#[test]
fn test_security_tls_cipher_suite_validation() {
    // SECURITY: Weak cipher suites must be rejected.
    // Only strong AEAD ciphers should be accepted.

    // Strong ciphers must be accepted
    assert!(
        TlsConfig::validate_cipher_suite("TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384").is_ok(),
        "Strong ECDHE+AES-GCM cipher must be accepted"
    );
    assert!(
        TlsConfig::validate_cipher_suite("TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384").is_ok(),
        "Strong ECDHE+RSA cipher must be accepted"
    );
    assert!(
        TlsConfig::validate_cipher_suite("TLS_CHACHA20_POLY1305_SHA256").is_ok(),
        "ChaCha20 cipher must be accepted"
    );

    // Weak ciphers with DES must be rejected
    let result = TlsConfig::validate_cipher_suite("TLS_RSA_WITH_DES_CBC_SHA");
    assert!(result.is_err(), "DES cipher must be rejected");
    let err = result.unwrap_err();
    assert!(err.contains("DES"), "Error should mention DES: {err}");

    // RC4 must be rejected
    assert!(
        TlsConfig::validate_cipher_suite("TLS_RSA_WITH_RC4_128_SHA").is_err(),
        "RC4 cipher must be rejected"
    );

    // MD5-based ciphers must be rejected
    assert!(
        TlsConfig::validate_cipher_suite("TLS_RSA_WITH_AES_128_CBC_MD5").is_err(),
        "MD5-based cipher must be rejected"
    );

    // NULL ciphers must be rejected
    assert!(
        TlsConfig::validate_cipher_suite("TLS_RSA_WITH_NULL_SHA").is_err(),
        "NULL cipher must be rejected"
    );

    // EXPORT ciphers must be rejected
    assert!(
        TlsConfig::validate_cipher_suite("TLS_RSA_EXPORT_WITH_RC4_40_MD5").is_err(),
        "EXPORT cipher must be rejected"
    );

    // Config with weak cipher in list must fail validation
    let config_with_weak = TlsConfig {
        enabled: true,
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ciphers: vec![
            "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384".to_string(),
            "TLS_RSA_WITH_RC4_128_SHA".to_string(), // weak
        ],
        ..TlsConfig::default()
    };
    let result = config_with_weak.validate();
    assert!(
        result.is_err(),
        "Config with weak cipher must fail validation"
    );
}

#[test]
fn test_security_tls_client_certificate_requirement() {
    // SECURITY: When mTLS is required, client certificate paths must be validated.

    // require_client_cert=true without ca_cert_path must be rejected
    let mtls_no_ca = TlsConfig {
        enabled: true,
        require_client_cert: true,
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ca_cert_path: None, // Missing — needed to verify client certs
        ..TlsConfig::default()
    };
    let result = mtls_no_ca.validate();
    assert!(
        result.is_err(),
        "mTLS without ca_cert_path must be rejected"
    );
    let errors = result.unwrap_err();
    assert!(
        errors.iter().any(|e| e.contains("ca_cert_path")),
        "Error should mention ca_cert_path: {errors:?}"
    );

    // require_client_cert=true with all paths must pass
    let mtls_valid = TlsConfig {
        enabled: true,
        require_client_cert: true,
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ca_cert_path: Some(PathBuf::from("/etc/ssl/certs/ca-bundle.crt")),
        ..TlsConfig::default()
    };
    assert!(
        mtls_valid.validate().is_ok(),
        "mTLS with all required paths must pass validation"
    );

    // require_client_cert=false should not require ca_cert_path
    let no_mtls = TlsConfig {
        enabled: true,
        require_client_cert: false,
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ca_cert_path: None,
        ..TlsConfig::default()
    };
    assert!(
        no_mtls.validate().is_ok(),
        "No mTLS should not require ca_cert_path"
    );
}

#[test]
fn test_security_tls_self_signed_cert_rejection() {
    // SECURITY: insecure_skip_verify must never be allowed in any configuration.
    // This prevents accepting self-signed certificates without proper CA validation.

    // insecure_skip_verify defaults to false
    let default_tls = TlsConfig::default();
    assert!(
        !default_tls.insecure_skip_verify,
        "insecure_skip_verify must default to false"
    );

    // Explicitly setting insecure_skip_verify=true must fail validation
    let insecure = TlsConfig {
        enabled: true,
        insecure_skip_verify: true,
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ca_cert_path: Some(PathBuf::from("/etc/ssl/certs/ca-bundle.crt")),
        ..TlsConfig::default()
    };
    let result = insecure.validate();
    assert!(
        result.is_err(),
        "insecure_skip_verify=true must always be rejected"
    );
    let errors = result.unwrap_err();
    assert!(
        errors.iter().any(|e| e.contains("insecure_skip_verify")),
        "Error must mention insecure_skip_verify: {errors:?}"
    );

    // Even with all other fields valid, insecure_skip_verify=true must fail
    let fully_configured_insecure = TlsConfig {
        enabled: true,
        insecure_skip_verify: true,
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ca_cert_path: Some(PathBuf::from("/etc/ssl/certs/ca-bundle.crt")),
        require_client_cert: true,
        client_cert_path: Some(PathBuf::from("/etc/ssl/certs/client.crt")),
        client_key_path: Some(PathBuf::from("/etc/ssl/private/client.key")),
        min_version: "TLSv1_3".to_string(),
        max_version: "TLSv1_3".to_string(),
        ciphers: vec!["TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384".to_string()],
    };
    assert!(
        fully_configured_insecure.validate().is_err(),
        "insecure_skip_verify=true must be rejected regardless of other settings"
    );
}

#[test]
fn test_security_tls_certificate_expiration_check() {
    // SECURITY: Certificate validation configuration must ensure that
    // expiration checking cannot be bypassed.
    //
    // Runtime certificate expiration is handled by the TLS library (rustls),
    // which rejects expired certificates by default. This test verifies that
    // the configuration cannot disable that behavior.

    // insecure_skip_verify (which would bypass expiration checks) must be rejected
    let config = TlsConfig {
        enabled: true,
        insecure_skip_verify: true,
        cert_path: Some(PathBuf::from("/etc/ssl/certs/server.crt")),
        key_path: Some(PathBuf::from("/etc/ssl/private/server.key")),
        ..TlsConfig::default()
    };
    let result = config.validate();
    assert!(
        result.is_err(),
        "Configuration that would bypass certificate expiration checks must be rejected"
    );

    // Valid config with proper cert verification enabled must pass
    let valid = valid_tls_config();
    assert!(
        valid.validate().is_ok(),
        "Valid config with certificate verification must pass"
    );
    assert!(
        !valid.insecure_skip_verify,
        "insecure_skip_verify must be false for valid config"
    );

    // Default TLS version must be 1.2+ (which enforces cert validation)
    let defaults = TlsConfig::default();
    assert_eq!(defaults.min_version, "TLSv1_2");
    assert_eq!(defaults.max_version, "TLSv1_3");
}

#[test]
fn test_security_http_endpoint_downgrade_prevention() {
    // SECURITY: When TLS is enabled, HTTP endpoints must be rejected
    // to prevent protocol downgrade attacks.

    let config = ReceiverConfig {
        tls: valid_tls_config(),
        ..ReceiverConfig::default()
    };

    // HTTP endpoint must be rejected when TLS is enabled
    let result = config.validate_endpoint("http://collector.example.com:4317");
    assert!(
        result.is_err(),
        "HTTP endpoint must be rejected when TLS is enabled"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("downgrade"),
        "Error should mention downgrade prevention: {err}"
    );

    // HTTPS endpoint must be accepted
    let result = config.validate_endpoint("https://collector.example.com:4317");
    assert!(
        result.is_ok(),
        "HTTPS endpoint must be accepted when TLS is enabled"
    );

    // When TLS is disabled, HTTP is allowed (no downgrade risk)
    let no_tls = ReceiverConfig {
        tls: TlsConfig::default(), // enabled=false
        ..ReceiverConfig::default()
    };
    let result = no_tls.validate_endpoint("http://collector.example.com:4317");
    assert!(
        result.is_ok(),
        "HTTP endpoint is allowed when TLS is not enabled"
    );

    // Mixed scenario: https_only + TLS disabled is caught by config validation
    let inconsistent = ReceiverConfig {
        https_only: true,
        tls: TlsConfig::default(),
        ..ReceiverConfig::default()
    };
    let result = inconsistent.validate();
    assert!(
        result.is_err(),
        "https_only without TLS enabled must fail validation"
    );
}
