//! Throwaway SSH keys for the SFTP integration tests.
//!
//! **These are not secrets.** They were generated once with the commands
//! below, for tests against a server that only ever listens on 127.0.0.1
//! inside a test process: as its host keys, and as the client keys that log
//! in to it. They are published in this repository. Never use them for
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
//! ssh-keygen -t rsa -b 2048 -m PEM -N blink-test -f rsa_pem_enc
//! ssh-keygen -t ecdsa -b 256 -m PKCS8 -N blink-test -f ecdsa_pkcs8_enc
//! ```
//!
//! And unencrypted keys in those two other formats, so the tests can show
//! a plain key in any format is never taken for one that needs a
//! passphrase:
//!
//! ```sh
//! ssh-keygen -t rsa -b 2048 -m PEM -N '' -f rsa_pem
//! ssh-keygen -t ecdsa -b 256 -m PKCS8 -N '' -f ecdsa_pkcs8
//! ```
//!
//! Legacy PEM and PKCS#8 keys hold no comment, so those commands give none.
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

/// RSA-2048 private key, legacy PEM (`Proc-Type: 4,ENCRYPTED`), encrypted.
pub(super) const RSA_PEM_KEY_ENCRYPTED: &str = r#"-----BEGIN RSA PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-128-CBC,0D63889AA258A36A7A34CE6E0D858ECB

k8VjhYqPWwP5KtHEPXA4js0sKkEFBPCGnmC6UsvQ8iLRphBp5C9x1xX2WsB8Q1gu
tLACiZBjdnAY+tBz/V6r9xa75vpgKa6qE+VA7xRDd7JQp65MJXWMj+hlEMe5V4nN
oQj5ZCoJCMBxQmnMcWQrVOkkWVfd5Rw9QnvqURJp5LnlxL6KaGIS1ufFpFkwAcZ0
VpCCYAM4rVrcDsIX0MKDsKzLLSa6xYERKLqFB0BVdh7PV4X8COPOxQ4hK1x18fui
kc3sj/Zk16P4kv/26jiPREyP0EOwmwklojf8JCu9O90LcgHu1eJSeytjXXg6TQHZ
4Nt0Xpf/036a+LJP5Bui7oSqT29van3jQhBeRGUn5S8KBjE7JfpTWboMqpJXIhiM
GtQV/TbRIOdz2MFoQTG5HuP+SGJDqzhGWWH6LCRyRnH+bXGDE81zQvBiGVziqtnu
XfW3OghdIl1X6m9vktzxtqYfwsi71aRUTdH+hhHyOE2EDR5epc2drsAbMdQX43pL
bNklaX/TUOxznI/NVEDRh3GO3WDLlBl+y7bJMvNhXNOObjctDg4NcAj/0Zt9x6nj
vOAM7TEpjjWCZYb0gFxlooqthhiC9EG9j4pvtuaclabr6cw428Nllky/oSpyLimt
rinqeSOt1pBYeGesoyvMZjSs5nWiGFM70SuCGE/zHwUCZ0Vr5Yo65oqFqKmJBu07
k4w6rqJhE4YK13l5KA93yNBDxdQ4sH8EB3G9VXkfcpPVxoTOnYRiwGXB4JUPHGHZ
maw3HkNCKYFh95Knm84wX6ImVsr15Y6EkYjwlvawbXrH5Q3HUO5E59h2kNLFvXiZ
UWC05ncpgVrxI42F+N8Xc2wJXBcBFYs5uyVasey5J041D1Phy1sU4JqMOX+ekVzT
FwGw4LU/omAHRXgDANFwhzRsAuWlB/wKJbfYJPO5O7tbe0CVxmPKmSlp9GBe/gwB
CulEvZqOCUwG10JbuRm/tQVx8R/bRBAnvNML0MuKCmxmGIWP6Asa8C50OUDEefx9
8/OoJ3zXyZhnq72rkCylLfbdjENlqPwBwiZ0lkDA/swNIF3C6jExfdiDlcyEFt9F
TTCLwyvwFZ7EJWP5L/Y8+QGF/KGi8CKkfZnpmbuOG4eYkvl7cGc/A/ujvUIc2t7E
rUPRjMCaxRqGCJhJz3A77YdTXsLgMqHW6nK/C99AOSATtntVM60nYn9lH1XhtWhV
gOG6n2QWfsTzoqD9nA9SGjJ8VJEyGu3GNNTmZjTLEC2NPvOaGcqxoziVH4jvbxaw
XfSZuS65dRF+bYeK9rzigheBmNyFXqxUEswCE7eA72dEIdgxhzL+a992gpi3Hvy9
1zhvAmub4V6mMb4UioGt6dE6XpA3Ot8al5+UwkVoRzd9KPR2LjwA4Yft7o/zmakT
chBP7kOVJvEVT70eRbSGoixwl7yPIdNY3JhAvUSGLfxXI+gETkYfBFwEp0ttA83g
Ogc9jiSMdAsDetc+QOrDSZ8H/W757STZA6ZXuxjwtJGcoVDVNrOnjx8knIDtYV0a
n4UZAi6P7aLnSck+7hJut7uVYVj7zd+/YtOGYRpJXkqU4btRBuI09xpOVUM1rXfC
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

/// RSA-2048 private key, legacy PEM (`BEGIN RSA PRIVATE KEY`), unencrypted.
pub(super) const RSA_PEM_KEY: &str = r#"-----BEGIN RSA PRIVATE KEY-----
MIIEpQIBAAKCAQEAqzzIeD+BqZ8OsAxkLHRzPB4c837zng+Qr1nKWRpCoL2olBr3
TgLSpDeZ/LbM99Y6/X9rzu7DLfI2tuaW7GwzZOs8Rwm5Qlz4xrq3+4sTDtdHfPSG
yTfKDH2ulJkMpY4iEB/o3AW48KdGql9v9n6n3O7hVUHf9UKecxUGdHpDRkDutE5m
vNiiUtd+RrFgTCjRo/VxCniogPC31+NVv2h0d6awGhz0vbUqJMWtjyMPVydTRC9x
CwtZ/o8jk74P8/YEUuaKZaClsWvK8ephH1t5pGtAnLaLLRDPS5TYFz7/AMr0bZO+
ADfvwlwl+WDuRXDHOxqtUSsOiFz8qaTAi05u8wIDAQABAoIBAA5Nl/MktC3Dvu7T
7M7NH/j+t5gGhnkxP0/I9hQbk5gomhBbm5pKYGeIy2smpm1HLuCQyE5dzDN7dA2H
/5rvmMVjuW0M8gq0iFp/1dwELGCEzpBbPnedRNpmlHK4wVaCI58+2Gz/FLO+ooea
9tewkaXsgCmRGeYQcqVrNVb9BWnfNx5k4i3pHglQVK8Tc05pNUX904j4gvaZFgVv
r1+Co2A6CGcioCdGbm6kkfVDOw0ZYH+ADZssvM/O1hLU/5ibx9NlBSpF0VUiNZdl
xAAW26E06UvM/YLRH9FJtDsOomQFgXke4x+uhKXFC61qYDXPp6N1CcafSyL8MArC
WtcWMzECgYEA56f9h7SXLzBgN+k18OTg7MDRcZNThZHFFxYrQVbYr1r4HSv9frap
7W/o8LYk8N2A/fuoQTd0KOT/kl5PNJkMITKbWXLZh7TnpeZrZymQQ4ZgaYcs89y5
3OhNLzjo7kFqNae9NBQ7Y+NXOPbu8ujUFkv5e2+IVkmvJfjadsFtm9ECgYEAvTto
g+2H91nBm61H0d22eib0AgQL0PUrbXJRXnuoJdAhuBu9rWLu8T1el4V0G7tXrW75
cdEoh9TXIZSQw9HJ2y74NiYibcsXX8bh96LuwhsOJRAVb3KisTWK6i/onTcCBdGV
JlMS1IPdcWVF5GAyJ6BicdU9WB8o6FTQ2qVwQ4MCgYEAhXt8EC0UDGHgn3bm2QbZ
QDKevMX6k3V6TqI8jGrBctGFNDH74H7ajXE1lTYvNQc9Y5wVyHZunX3OcgWbuCAU
KDDj6PfPtm2cKTELZCdD9FTCyyvT0DS4eDz29KXjOh5BV+duk0qxKg5+XNmUuC1G
wA5JFEIhGi9dGBE7uMAeK3ECgYEAmO0A2tVEM1ZbC5du2iCmTnNacKKadgUcUne3
YHbofZ6FEYeBszSny1eAMU2Ndrfg9G0hs6SHb7ZHIYExuL66taJwvzn0Sux8+E7+
4KTQ9RbuSG+GEyn+Zt3RZM7Q2vTQcqlcMruiFRJ4DCL+58AB6FpKFJpy19tesNuS
aZCXeEcCgYEAshLo/iuJyVm+ypYsLXr9pt7qfW8t8g1pgNQC0Z++v50BCO8DlTef
PUFmWp634zop+znUUt0SzMgbxuCu1U0Xek8HmplijgLHYwHYvwzvO0dqp5AVe724
IlhRtrgBGY4X5EE5PyWIzEheFKCtwtWcgyb3lXL9S5A03QM/JBul29s=
-----END RSA PRIVATE KEY-----
"#;

/// ECDSA P-256 private key, PKCS#8 (`BEGIN PRIVATE KEY`), unencrypted.
pub(super) const ECDSA_PKCS8_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgtXO4eG8C89XSlwOZ
RCGHhVvYafgC+4bkpiblKx++RnShRANCAARFLBGFrYcL/hbgCil26IhSW8DwIXvH
84Xw8gFEsGmjQMMm5ks7V9yuEuZStsQiC1sWPRYk9ImQ0idcGnCfHHoO
-----END PRIVATE KEY-----
"#;
