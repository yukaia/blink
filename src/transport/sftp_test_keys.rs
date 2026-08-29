//! Throwaway SSH keys for the SFTP integration tests.
//!
//! **These are not secrets.** They were generated once with the commands
//! below, for a server that only ever listens on 127.0.0.1 inside a test
//! process, and they are published in this repository. Never use them for
//! anything.
//!
//! ```sh
//! ssh-keygen -t ed25519 -N '' -C blink-test-ed25519 -f ed25519
//! ssh-keygen -t ecdsa -b 256 -N '' -C blink-test-ecdsa -f ecdsa
//! ssh-keygen -t rsa -b 2048 -N '' -C blink-test-rsa -f rsa
//! ```
//!
//! They are fixtures rather than generated per run because RSA keygen is
//! ruinously slow: 19.7 s for one 2048-bit key through `PrivateKey::random`,
//! measured against a suite that runs in under a second, and the cost varies
//! run to run because keygen searches for primes. Ed25519 (0.4 ms) and ECDSA
//! P-256 (6 ms) would be affordable to generate, but keeping all three as
//! fixtures keeps one code path and makes the tests deterministic.

/// Ed25519 private key, OpenSSH format.
pub(super) const ED25519_KEY: &str = r#"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACATcrf8fWXL2NfWnudHxGgbYyZ+pBJcIvFxco9Jh16xkQAAAJiCGnl/ghp5
fwAAAAtzc2gtZWQyNTUxOQAAACATcrf8fWXL2NfWnudHxGgbYyZ+pBJcIvFxco9Jh16xkQ
AAAECwh9SC9UT7Ln4Jt+TYuaoGm6IQ3MyECZUD+0blTQdjcRNyt/x9ZcvY19ae50fEaBtj
Jn6kElwi8XFyj0mHXrGRAAAAEmJsaW5rLXRlc3QtZWQyNTUxOQECAw==
-----END OPENSSH PRIVATE KEY-----
"#;

/// ECDSA P-256 private key, OpenSSH format.
pub(super) const ECDSA_KEY: &str = r#"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNlY2RzYS
1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQRSB3S9egXRNyYReVwXseb6GUEmSsaJ
ImWHL2qft03YagTtFdkZpyyO5eqloiCvconJ64vbvyUwApTNTuMErX18AAAAqKa8wV6mvM
FeAAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBFIHdL16BdE3JhF5
XBex5voZQSZKxokiZYcvap+3TdhqBO0V2RmnLI7l6qWiIK9yicnri9u/JTAClM1O4wStfX
wAAAAgBdrUdLoiJ/taYohquMdRj5neTKvAdAZEJFjE2Cj9ploAAAAQYmxpbmstdGVzdC1l
Y2RzYQ==
-----END OPENSSH PRIVATE KEY-----
"#;

/// RSA-2048 private key, OpenSSH format.
pub(super) const RSA_KEY: &str = r#"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAABFwAAAAdzc2gtcn
NhAAAAAwEAAQAAAQEAsgTu1hgcEBdcu0Zr5Xkk0N+SrssULsRz4pA3vSkEMx45Wg0O5ZC0
IBMHJehask/x2THcSCbrbiryWyMSJc73ohROnMUWnSLE1ktEx89mZVpk6PBwVQlrrt5nNc
VucmWFZ/ljS+d/y7UW2UY4iRI7JPh04/OEYQJfRP29Q/oVW+E9JE00EkFpU3pGENgvK8fH
LyoLPxaeoLF5ffU/SzzSOeua+Qa0IJ0TMQq+EZ1/x2nnlEEZ8hyvdwr16AdE5qCDCc9IDS
SVc53eWsv+hHZqzNtkuedQ+2iKZS0Jkchz68AwDM76ymXeuxoRpfThjVyr24cfkwAPmDV9
EyH8Khy9XQAAA8iN/Wqjjf1qowAAAAdzc2gtcnNhAAABAQCyBO7WGBwQF1y7RmvleSTQ35
KuyxQuxHPikDe9KQQzHjlaDQ7lkLQgEwcl6FqyT/HZMdxIJutuKvJbIxIlzveiFE6cxRad
IsTWS0THz2ZlWmTo8HBVCWuu3mc1xW5yZYVn+WNL53/LtRbZRjiJEjsk+HTj84RhAl9E/b
1D+hVb4T0kTTQSQWlTekYQ2C8rx8cvKgs/Fp6gsXl99T9LPNI565r5BrQgnRMxCr4RnX/H
aeeUQRnyHK93CvXoB0TmoIMJz0gNJJVznd5ay/6EdmrM22S551D7aIplLQmRyHPrwDAMzv
rKZd67GhGl9OGNXKvbhx+TAA+YNX0TIfwqHL1dAAAAAwEAAQAAAQATMYj2uF6+NWagInWb
pjYb9x7/jZG9gRzlfpsj3/o98LJKTUIf6jwhgSuyIJ02wHvY6RFRDjEwDZ1Xyi44uVnltb
7MFEvd4VPLrw3ZZTkrEFX074eNA5kCn6QNHh5MYznA/hiApJMYyYuPHY0W6kpKMCeaNDU/
qFvROnJfk+UdpLrqDnNicSgt3uLF/cG7LO8ji2jia+IjeBhOaKOuW7a30ze510Cx1dlV1x
KT0oCFtLHg4JsA9RNyaskQO4oEdXCBPHIHkO/wMcfF8ZMIUiRNIsvKCiLScveNN6OAw7fL
ZpNGyBSmpL+CE2sKi09EYb9NuvHku7vJLxeLMVhcN1EBAAAAgARsYjiGfsVaSUWm7irAfu
lnMHybl6WP/+POk41ZXdgXg0kBW/ghOKVgmNu08RTqtEiJUlNZ6WjutaOtK6WSDIK1a4Ql
hr9XJaOSg6cPPafOrneRbN2rNop6HQd3lhmerBqQK1LDjTxZxTbQ+cq+aCDfwB1Cv4lu4X
tCSW+7b6QgAAAAgQDxkZp3JCXTxsCbsXMuTAIvQFIdwpV7Qs/sdNZRcZ50SzadwjWjU/aX
nGYBdL7gr7cpq7QHMVfIIEkY5Fyij5akxWw1J1MM38oEZgaDcpn1P7x+3SDzubhyKo/6Ni
bq9NHbBYx24jS8heQBwArLakUjqqZA69Eyoh19L6l/THgBfQAAAIEAvKdx1ZtESLSKG9AM
1H65aKwECkJKoNFfkQCuZV9UIRVCAwuF1UVGpg2i6P+FRCXtvTMFpoOcPTjSsVtSJt5pso
yq1eLilFjtk2Pyu5ALGcVh0S5d/bvKQKefR7eJv3x1870DD6fMDAFedEGQK73Brb50oO3Y
QUjiIrs5LJiscWEAAAAOYmxpbmstdGVzdC1yc2EBAgMEBQ==
-----END OPENSSH PRIVATE KEY-----
"#;
