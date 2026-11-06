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
//! The encrypted ones, passphrase [`TEST_PASSPHRASE`], one per container
//! format a passphrase-protected key can come in:
//!
//! ```sh
//! ssh-keygen -t ed25519 -N blink-test -C blink-test-ed25519-encrypted -f ed25519_enc
//! ssh-keygen -t rsa -b 1024 -m PEM -N blink-test -C x -f rsa_pem_enc
//! ssh-keygen -t ecdsa -b 256 -m PKCS8 -N blink-test -C x -f ecdsa_pkcs8_enc
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

/// The passphrase of the `*_ENCRYPTED` keys below.
pub(super) const TEST_PASSPHRASE: &str = "blink-test";

/// Ed25519 private key, OpenSSH format, encrypted.
pub(super) const ED25519_KEY_ENCRYPTED: &str = r#"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABDm6Y81+9
LuKZRpEqNVa2rsAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIO69nfRYLFWhEI5y
Zyu21T6cOPcIH90HtN1hiJ3qyHP5AAAAoMQHXLIGg00ePsY/efrWBozWRHzrOnObKSKMpg
qXLWJmxT4sFUXLjyd84V4mXoqMJSijcLPCxA73OTkgu1iE8E4keMGl1hYXeIK0jrgnWDJW
BRcf4wVP3WAkea2moHfSOSZFhqlZCImdtNEjkw7h5/SzAW/RsqrQVcYn4GamMeaBranTHo
ZxelkWu+KjM9PIzH/goFKh8t82ACtQZkMb6aM=
-----END OPENSSH PRIVATE KEY-----
"#;

/// RSA private key, legacy PEM (`Proc-Type: 4,ENCRYPTED`), encrypted.
pub(super) const RSA_PEM_KEY_ENCRYPTED: &str = r#"-----BEGIN RSA PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-128-CBC,57E8943BD9A77FE49E6B933170B902E5

1bDTdT4o+HBqR8JGvmmtOU7HoaOLF6xoYHfTaTStFh3TGdFgX4/0ParGyOBGBaEw
dkALf2Ecy3mNfYyyNaskpvL4uMLiE5/j3D5A0ctqkbJ2Tk20AxEsnQ/CB8Jnj7Wd
sQmIwdS828n6TxM/BbwaA5atUF7GxGw5xIHHXRQb6fUNBFPQatHRiP3pRjhZiJJO
n4EN+7ZYPqmsfHgk9rC1BX/xk5NX/3HjRckTXaqSsgh7B9QaJgJlUrRBMYbRWZUL
d1MO/aU6Nb7bG4Oic3+2t/xVzQplReq+eig5l4bObTiSgJUjwIkgq5C6JYhC/F58
NCTSBY6y/Ix8ZCMKrYG7+XVt2aI9N+54Li3Zn+Rd9sIcmV+oX0f78W1TQcOATHUK
O9eK/iQdFtZDoP3RQ7xsYFiHM+lsvaEw8VjFJGz8J3rEqNLuiubobum0BdMDqyNG
jIjdWab3rI/eeI73CsZayyLbcTGOLv7mBGZADwEyB3g5e8epmJ4GkraYnqgICvjK
83DlU2qky3ZbBVm0lrtBIV/A6pz8MVU07E695T3xyiufd9FcMyzK8lFKHz2+Gl6o
56o/WsD6TINT0OTsqAiw2PM6dBTwkNgGkn496wOCRlyyHUCCf/RPdTgY1bUUb43G
mq6H/biO+dpKUzo5J3LtNTDCuJPbCBbjgOho/jGP4i8okYRTpcc4J99aR1RGC8gI
VvqN9yo2MjMDM7IAMdeqfyDAVeadqFzaV1NKPpiQogb0dBFR7F6zFpklWccaXOO3
L9g8YKkE2URI/35N5hVYdeh7PFswk/5SWBEjMWoz3K4SGLAC1s1GSRUTaiNEBLkp
-----END RSA PRIVATE KEY-----
"#;

/// ECDSA P-256 private key, PKCS#8 (`ENCRYPTED PRIVATE KEY`).
pub(super) const ECDSA_PKCS8_KEY_ENCRYPTED: &str = r#"-----BEGIN ENCRYPTED PRIVATE KEY-----
MIH0MF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBClDmyjqtjff3RFz0tp
iWS7AgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBAgQQMwRO7PD/dtBG0zbN
uSPp5QSBkAZOc8uaZxeOd0ok+QF6j2olTIf46GEqmvJ7zDoUXeHi/g5pJE5kPHYT
rLwZDEZH1FDvTBHxUkzbk8m6VGzE4lJpXIPuS2BC0XbWmMffkZyQKLU7Tm8HjDyJ
CSJrAt5DyJO9eSYngvtFDi/VijHDQM1kLY0Ft++x8Nusjkzftyb4M6E8sMTvyGY8
BOiOJ/w9YA==
-----END ENCRYPTED PRIVATE KEY-----
"#;
