//! Throwaway TLS certificates for the FTPS integration tests.
//!
//! **These are not secrets.** They were generated once, for a server that
//! only ever listens on loopback inside a test process, and they are
//! published in this repository. Never use them for anything.
//!
//! They are fixtures rather than generated per run so the tests need no
//! certificate generator among the dev-dependencies. Each is ECDSA P-256,
//! valid from 2026-01-01 until 2100, stored as base64 of the DER: the
//! certificate, and its key as PKCS#8. They were made with this program
//! (`rcgen` 0.14, `time` 0.3, `base64` 0.22), which prints `NAME base64`
//! lines:
//!
//! ```ignore
//! use base64::Engine as _;
//! use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
//! use time::{Date, Month};
//!
//! fn params(cn: &str, sans: &[&str]) -> CertificateParams {
//!     let sans = sans.iter().map(|s| s.to_string()).collect::<Vec<_>>();
//!     let mut p = CertificateParams::new(sans).unwrap();
//!     p.distinguished_name.push(DnType::CommonName, cn);
//!     p.not_before = Date::from_calendar_date(2026, Month::January, 1).unwrap().midnight().assume_utc();
//!     p.not_after = Date::from_calendar_date(2100, Month::January, 1).unwrap().midnight().assume_utc();
//!     p
//! }
//!
//! fn out(name: &str, cert: &[u8], key: Option<&KeyPair>) {
//!     let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
//!     println!("{name}_CERT {}", b64(cert));
//!     if let Some(k) = key {
//!         println!("{name}_KEY {}", b64(&k.serialize_der()));
//!     }
//! }
//!
//! fn main() {
//!     let local = ["127.0.0.1", "::1", "localhost"];
//!     for n in ["A", "B"] {
//!         let k = KeyPair::generate().unwrap();
//!         let cert = params(&format!("blink test {n}"), &local).self_signed(&k).unwrap();
//!         out(n, cert.der(), Some(&k));
//!     }
//!     let k = KeyPair::generate().unwrap();
//!     let cert = params("blink test OTHER_NAME", &["other.example"]).self_signed(&k).unwrap();
//!     out("OTHER_NAME", cert.der(), Some(&k));
//!     let k = KeyPair::generate().unwrap();
//!     out("CN_ONLY", params("127.0.0.1", &[]).self_signed(&k).unwrap().der(), Some(&k));
//!
//!     let ca_key = KeyPair::generate().unwrap();
//!     let mut ca = params("blink test CA", &[]);
//!     ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
//!     ca.key_usages = vec![KeyUsagePurpose::KeyCertSign];
//!     out("CA", ca.self_signed(&ca_key).unwrap().der(), None);
//!     let issuer = Issuer::new(ca, ca_key);
//!     let k = KeyPair::generate().unwrap();
//!     let cert = params("blink test CA_SIGNED", &local).signed_by(&k, &issuer).unwrap();
//!     out("CA_SIGNED", cert.der(), Some(&k));
//!     let k = KeyPair::generate().unwrap();
//!     let cert = params("blink test CA_SIGNED_OTHER_NAME", &["other.example"]).signed_by(&k, &issuer).unwrap();
//!     out("CA_SIGNED_OTHER_NAME", cert.der(), Some(&k));
//! }
//! ```

/// Names `127.0.0.1`, `::1` and `localhost`.
pub const A_CERT: &str = "MIIBYjCCAQigAwIBAgIUUlN+8OFpGZC5U3BImYf0WSk84jcwCgYIKoZIzj0EAwIwFzEVMBMGA1UE\
     AwwMYmxpbmsgdGVzdCBBMCAXDTI2MDEwMTAwMDAwMFoYDzIxMDAwMTAxMDAwMDAwWjAXMRUwEwYD\
     VQQDDAxibGluayB0ZXN0IEEwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAToLJ9R1LYsq30gcWxh\
     mEexJrqCrj66Ssqz8HDw41z3mzW4WZHFt76Uqtv+A66ZCBI0u+rhy3ukrHCwxSS+Z5a1ozAwLjAs\
     BgNVHREEJTAjhwR/AAABhxAAAAAAAAAAAAAAAAAAAAABgglsb2NhbGhvc3QwCgYIKoZIzj0EAwID\
     SAAwRQIgYP3Nf0qlx5ZL/CIbJQKUU2F+AIfVd9VKDry1R3X08MYCIQCOB1hvI07Su/slxbSMaxf7\
     ngG+LCiFqSQgp7dC9wU9ew==";
/// Key of [`A_CERT`].
pub const A_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgZNSQRhi07UJHRmZXLzEEsYVuSYMi\
     JFtGjCIO1TXxE/ChRANCAAToLJ9R1LYsq30gcWxhmEexJrqCrj66Ssqz8HDw41z3mzW4WZHFt76U\
     qtv+A66ZCBI0u+rhy3ukrHCwxSS+Z5a1";

/// Names `127.0.0.1`, `::1` and `localhost`, as [`A_CERT`] does, with another key.
pub const B_CERT: &str = "MIIBYTCCAQigAwIBAgIUemEsuLQOS2bNUOZrGvFr8YQj+8QwCgYIKoZIzj0EAwIwFzEVMBMGA1UE\
     AwwMYmxpbmsgdGVzdCBCMCAXDTI2MDEwMTAwMDAwMFoYDzIxMDAwMTAxMDAwMDAwWjAXMRUwEwYD\
     VQQDDAxibGluayB0ZXN0IEIwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASSG4sBvVyh11QYSpa/\
     6ydPfW3irUk4+8FQK2wooIVhmx4cyYma78Uc8xj9KJuZkelNVUIYlunn+OOImxl49UpmozAwLjAs\
     BgNVHREEJTAjhwR/AAABhxAAAAAAAAAAAAAAAAAAAAABgglsb2NhbGhvc3QwCgYIKoZIzj0EAwID\
     RwAwRAIgNjnl5qtQ8kJRJ+OA83BUEkZyceObPInffTe5aIVOeHoCIB29yyUzehU6iiAtoEv7Ozh+\
     tbtWlGkfhi0YyAPJ4Wy5";
/// Key of [`B_CERT`].
pub const B_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgqnZ2eilkXajNWea4lowI+o9PBgny\
     Au4nWyQI651jZiShRANCAASSG4sBvVyh11QYSpa/6ydPfW3irUk4+8FQK2wooIVhmx4cyYma78Uc\
     8xj9KJuZkelNVUIYlunn+OOImxl49Upm";

/// Names `other.example` only, so not the host the tests connect to.
pub const OTHER_NAME_CERT: &str = "MIIBXzCCAQagAwIBAgIUJmNqYodkuKfBSQMXMGRtAeyc76EwCgYIKoZIzj0EAwIwIDEeMBwGA1UE\
     AwwVYmxpbmsgdGVzdCBPVEhFUl9OQU1FMCAXDTI2MDEwMTAwMDAwMFoYDzIxMDAwMTAxMDAwMDAw\
     WjAgMR4wHAYDVQQDDBVibGluayB0ZXN0IE9USEVSX05BTUUwWTATBgcqhkjOPQIBBggqhkjOPQMB\
     BwNCAARaZeWm3DcSqAIwiy0yHaQoKdBQeGBlBvaUzMa95w/VLDEp9BXAhHCC0etGJQ6VqPg+1GBK\
     uRsu6hVMMjOJl0mPoxwwGjAYBgNVHREEETAPgg1vdGhlci5leGFtcGxlMAoGCCqGSM49BAMCA0cA\
     MEQCIHacMVV5xYfz4onDS7HzKAOAdpT7AO3kcL8HYIKR88jvAiBr9LEysYgYFOFxlXNnUeCy1YXx\
     uQqQVSOklm6hsg2BlQ==";
/// Key of [`OTHER_NAME_CERT`].
pub const OTHER_NAME_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgzhjEqDQs3Imu7CkJDM0OQE5TPXOz\
     lSl5pjwKAx2sA9GhRANCAARaZeWm3DcSqAIwiy0yHaQoKdBQeGBlBvaUzMa95w/VLDEp9BXAhHCC\
     0etGJQ6VqPg+1GBKuRsu6hVMMjOJl0mP";

/// Common name `127.0.0.1` and no subject alternative names, as a bare
/// `openssl req -x509 -subj /CN=…` makes. rustls reads only the SANs, so
/// as far as it is concerned this names no host at all.
pub const CN_ONLY_CERT: &str = "MIIBKTCB0KADAgECAhQynpaJnNCLgKCs6mHhkpeStAgt+DAKBggqhkjOPQQDAjAUMRIwEAYDVQQD\
     DAkxMjcuMC4wLjEwIBcNMjYwMTAxMDAwMDAwWhgPMjEwMDAxMDEwMDAwMDBaMBQxEjAQBgNVBAMM\
     CTEyNy4wLjAuMTBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABIPFoQPXt2D844qA5JXS8rgQ0aDE\
     yuOG17QSiRjGKTK0FBUvXwpatdFo8yVB5CvNfJdallIcfYHh4NMC13TxghgwCgYIKoZIzj0EAwID\
     SAAwRQIhAJgIOurVPETnWOwpI0GEcc8CjcH2HmLntekIRPdNsvTAAiAxfyEwzZytgnKGgtiv33ot\
     3UWnqfZNokugqpL9D8EM/w==";
/// Key of [`CN_ONLY_CERT`].
pub const CN_ONLY_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgp6L+wxqHbWjL8AO0qkWPmfHk9ibu\
     XM2MjDHRaGCkQkShRANCAASDxaED17dg/OOKgOSV0vK4ENGgxMrjhte0EokYxikytBQVL18KWrXR\
     aPMlQeQrzXyXWpZSHH2B4eDTAtd08YIY";

/// A certificate authority, standing in for one installed in the system's
/// trust store. Its key was thrown away once it had signed the two below.
pub const CA_CERT: &str = "MIIBdzCCARygAwIBAgIUMdlf66xinf6629k4VDkylxveQMwwCgYIKoZIzj0EAwIwGDEWMBQGA1UE\
     AwwNYmxpbmsgdGVzdCBDQTAgFw0yNjAxMDEwMDAwMDBaGA8yMTAwMDEwMTAwMDAwMFowGDEWMBQG\
     A1UEAwwNYmxpbmsgdGVzdCBDQTBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABNmEeKpCsGmYEtsy\
     dZT3RmMX9ChmbTXPePMrMWmbe32tMV0DYgvOnhDhiIi9rGJ7OBvJLbVNG1EbpShyktuFqc6jQjBA\
     MA4GA1UdDwEB/wQEAwICBDAdBgNVHQ4EFgQUtPtECvvfjtYGLFT/9rwMuRdepq0wDwYDVR0TAQH/\
     BAUwAwEB/zAKBggqhkjOPQQDAgNJADBGAiEA+bcmEp2dpHq2A4EWYMiLI5HO1CxlQY1FKQzv5uM3\
     sZYCIQDrDuMUEQAb1d4vUVMMBSYmXgWQgoE1YoPhFWBxGjG7Sw==";

/// Names `127.0.0.1`, `::1` and `localhost`, issued by [`CA_CERT`].
pub const CA_SIGNED_CERT: &str = "MIIBazCCARGgAwIBAgIUaviADhySPi1pbIc13WIegDeyETMwCgYIKoZIzj0EAwIwGDEWMBQGA1UE\
     AwwNYmxpbmsgdGVzdCBDQTAgFw0yNjAxMDEwMDAwMDBaGA8yMTAwMDEwMTAwMDAwMFowHzEdMBsG\
     A1UEAwwUYmxpbmsgdGVzdCBDQV9TSUdORUQwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASYR3gx\
     950O79sB19nRqridMRKwZHCTki5CKuAXPkMK+yAG5+58K5dvvusnnar3Yim1TBpIAMXDTUWeamaU\
     XTvyozAwLjAsBgNVHREEJTAjhwR/AAABhxAAAAAAAAAAAAAAAAAAAAABgglsb2NhbGhvc3QwCgYI\
     KoZIzj0EAwIDSAAwRQIgTRuCDSpBha3Dz9QxvNFG21G8ScdROh7A/J1O+v0pVp4CIQD83DEZVeeo\
     8ePPqt/q1KsYaYh/kj2ZIFfIinFvvQhlEw==";
/// Key of [`CA_SIGNED_CERT`].
pub const CA_SIGNED_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgRTLgN9TXG0Tj1fh7jXwAk6itb51U\
     hA1N63UDvf2B8KChRANCAASYR3gx950O79sB19nRqridMRKwZHCTki5CKuAXPkMK+yAG5+58K5dv\
     vusnnar3Yim1TBpIAMXDTUWeamaUXTvy";

/// Names `other.example` only, issued by [`CA_CERT`].
pub const CA_SIGNED_OTHER_NAME_CERT: &str = "MIIBYzCCAQigAwIBAgIUBlnJGukN6U1l6RvxQNH5YUa2C9kwCgYIKoZIzj0EAwIwGDEWMBQGA1UE\
     AwwNYmxpbmsgdGVzdCBDQTAgFw0yNjAxMDEwMDAwMDBaGA8yMTAwMDEwMTAwMDAwMFowKjEoMCYG\
     A1UEAwwfYmxpbmsgdGVzdCBDQV9TSUdORURfT1RIRVJfTkFNRTBZMBMGByqGSM49AgEGCCqGSM49\
     AwEHA0IABFYsdWEslOJJ7Dkybxq4eML6vSRbLldXD8MnEZpD+BjkjkNEuDmfok0yBhKpNdYdN6PT\
     kv9VVhzYS1jKNOVRyE+jHDAaMBgGA1UdEQQRMA+CDW90aGVyLmV4YW1wbGUwCgYIKoZIzj0EAwID\
     SQAwRgIhAI4dVTT4jEZZGzXH2O2beY6fPxgrsnzHfZ2Hf2w0Rx0CAiEAprapmKZ3tbwyO4gznUmF\
     ef/LK3TaG1CGFw6QBiMErtY=";
/// Key of [`CA_SIGNED_OTHER_NAME_CERT`].
pub const CA_SIGNED_OTHER_NAME_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgi9Bv2EMax62loXxzaeTIBzo65GfE\
     G5X6rnbm/OVOt+ShRANCAARWLHVhLJTiSew5Mm8auHjC+r0kWy5XVw/DJxGaQ/gY5I5DRLg5n6JN\
     MgYSqTXWHTej05L/VVYc2EtYyjTlUchP";
