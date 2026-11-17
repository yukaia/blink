//! Throwaway TLS certificates for the FTPS integration tests.
//!
//! **These are not secrets.** They were generated once, for a server that
//! only ever listens on 127.0.0.1 inside a test process, and they are
//! published in this repository. Never use them for anything.
//!
//! Each is a self-signed ECDSA P-256 certificate valid until 2100, made
//! with `rcgen` 0.14 (`CertificateParams::new(sans)`, `self_signed`), and
//! stored as base64 of the DER: the certificate, and its key as PKCS#8.
//!
//! They are fixtures rather than generated per run so the tests need no
//! certificate generator among the dev-dependencies.

/// Names `127.0.0.1` and `localhost`.
pub const A_CERT: &str = "MIIBTzCB9qADAgECAhRkJrWH2/NieAGVmWBhTclgNXjn4jAKBggqhkjOPQQDAjAXMRUwEwYDVQQD\
     DAxibGluayB0ZXN0IEEwIBcNMjMxMTE0MjIxMzIwWhgPMjEwMDAxMDEwMDAwMDBaMBcxFTATBgNV\
     BAMMDGJsaW5rIHRlc3QgQTBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABCIRPWr5kexHUU2huBBN\
     Ri3rWcdwuLB3p3PfTq2RK+/7MkumEZz/LgT4/5cwTXi8ECmzeMELzB8G2KvUH5IMCNGjHjAcMBoG\
     A1UdEQQTMBGHBH8AAAGCCWxvY2FsaG9zdDAKBggqhkjOPQQDAgNIADBFAiANRxpI6IeqcaO6/90K\
     3PtniOkXmz16QfbChgGIcYKqyQIhAOSbH4pkB/ingM4lt08Dt+z2PoRHan6Od3wEtGYiw1yv";
/// Key of [`A_CERT`].
pub const A_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgn7N164H8JOUwCsUcqDXxSYhuWXwj\
     eqzFYZjV62hBK2+hRANCAAQiET1q+ZHsR1FNobgQTUYt61nHcLiwd6dz306tkSvv+zJLphGc/y4E\
     +P+XME14vBAps3jBC8wfBtir1B+SDAjR";

/// Names `127.0.0.1` and `localhost`, as [`A_CERT`] does, with another key.
pub const B_CERT: &str = "MIIBTjCB9qADAgECAhRe5Ufw6aX6epWUjGQn9axvlf4QHzAKBggqhkjOPQQDAjAXMRUwEwYDVQQD\
     DAxibGluayB0ZXN0IEIwIBcNMjMxMTE0MjIxMzIwWhgPMjEwMDAxMDEwMDAwMDBaMBcxFTATBgNV\
     BAMMDGJsaW5rIHRlc3QgQjBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABGUEZE11eYIHqaMyHg8z\
     JYjQBRWcIx+yZI+SXpn6giQsd+48fYFyLgNLmL+pVfciPW+19hJ5ntG6vC0pXCxYcf+jHjAcMBoG\
     A1UdEQQTMBGHBH8AAAGCCWxvY2FsaG9zdDAKBggqhkjOPQQDAgNHADBEAiAkHsYOTOCu74qaLxYb\
     6iULTcu1VUtzjMsOsv4E7wLqzQIgXOcqQEnZvNSpFX45f6nCEuUFzX3rd7r5mizzp1KKSXc=";
/// Key of [`B_CERT`].
pub const B_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg733lcA8y9EAR7TxX+1t/MsX+K88Y\
     LyQjeGsYk7iXPl+hRANCAARlBGRNdXmCB6mjMh4PMyWI0AUVnCMfsmSPkl6Z+oIkLHfuPH2Bci4D\
     S5i/qVX3Ij1vtfYSeZ7RurwtKVwsWHH/";

/// Names `other.example` only, so not the host the tests connect to.
pub const OTHER_NAME_CERT: &str = "MIIBYDCCAQagAwIBAgIUVvAzg74N3WygJczPaUh4rMgy6J4wCgYIKoZIzj0EAwIwIDEeMBwGA1UE\
     AwwVYmxpbmsgdGVzdCBPVEhFUl9OQU1FMCAXDTIzMTExNDIyMTMyMFoYDzIxMDAwMTAxMDAwMDAw\
     WjAgMR4wHAYDVQQDDBVibGluayB0ZXN0IE9USEVSX05BTUUwWTATBgcqhkjOPQIBBggqhkjOPQMB\
     BwNCAAQ0BCTB1aueAyaBF9ci/LW6/jlnhjzWAKc+dFjdS7/bnGnHXwdJoP2yywzsnfquEYGSHUiD\
     gn7b9/4sbVokNApBoxwwGjAYBgNVHREEETAPgg1vdGhlci5leGFtcGxlMAoGCCqGSM49BAMCA0gA\
     MEUCIQDAJeyr4+CeXmbRdSebOt1Q/twZyS08uJm5JZCBdwwxXAIgfGoP6vPTn+IImU66gweLgBgk\
     dyFGXLCRAv9J/tZaUog=";
/// Key of [`OTHER_NAME_CERT`].
pub const OTHER_NAME_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgvuOykFZN8vR6H37KKHtfbXDsmmxS\
     k2N7k7Gc/0NN5kGhRANCAAQ0BCTB1aueAyaBF9ci/LW6/jlnhjzWAKc+dFjdS7/bnGnHXwdJoP2y\
     ywzsnfquEYGSHUiDgn7b9/4sbVokNApB";
