#!/usr/bin/env bash
# Regenerates the S/MIME fixtures used by the unit tests in mail/smime.rs (OpenSSL 3).
# Everything here is test material: the keys are published and protect nothing.
#
#   bash crates/ferrite-core/src/mail/testdata/smime/generate.sh
#
# Files: ca.pem (test CA), {alice,bob}.pem/.key (RSA 2048, emailProtection, SAN email),
# alice.p12 (modern PBES2/AES-256 + HMAC-SHA256, password "alice-p12"), alice-legacy.p12
# (3DES + RC2-40, as old exports), carol-ec.pem/.key (P-256, for ECDSA signatures),
# expired.pem/.key, and messages made by `openssl cms`: signed-detached.eml (multipart/signed),
# signed-opaque.eml, encrypted-aes.eml (AES-256-CBC to Alice), encrypted-3des.eml (DES-EDE3-CBC
# to Alice), encrypted-oaep.eml (RSAES-OAEP key transport), signed-encrypted.eml (signed by
# Bob, then encrypted to Alice), signed-ec.eml (signed by Carol's P-256 key, SHA-384), encrypted-ber.eml (streamed:
# indefinite-length BER, as NSS and other streaming encoders write it).
set -euo pipefail
cd "$(dirname "$0")"
rm -f ./*.pem ./*.key ./*.p12 ./*.eml ./*.srl ./*.cnf ./*.csr

days=36500
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days "$days" -subj "/CN=Ferrite Test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null

leaf() { # name email keyspec [days]
  local name=$1 email=$2 keyspec=$3 valid=${4:-$days}
  cat >"$name.cnf" <<EOF
basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=emailProtection
subjectAltName=email:$email
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid
EOF
  openssl req -newkey "$keyspec" -nodes -keyout "$name.key" -out "$name.csr" -subj "/CN=${name^}/emailAddress=$email" 2>/dev/null
  openssl x509 -req -in "$name.csr" -CA ca.pem -CAkey ca.key -CAcreateserial -out "$name.pem" -days "$valid" -extfile "$name.cnf" 2>/dev/null
  rm -f "$name.csr" "$name.cnf"
}
leaf alice alice@example.test rsa:2048
leaf bob bob@example.test rsa:2048
leaf carol-ec carol@example.test ec:<(openssl ecparam -name prime256v1)
# Expired since 2020 (openssl ca can backdate; `x509 -req` cannot in OpenSSL 3.0).
mkdir -p ca.db && : >ca.db/index.txt && echo 1000 >ca.db/serial
cat >ca.cnf <<EOF
[ca]
default_ca = test
[test]
dir = ca.db
database = ca.db/index.txt
serial = ca.db/serial
new_certs_dir = ca.db
certificate = ca.pem
private_key = ca.key
default_md = sha256
policy = any
copy_extensions = none
x509_extensions = leaf
[any]
commonName = supplied
[leaf]
basicConstraints = CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = emailProtection
subjectAltName = email:old@example.test
EOF
openssl req -newkey rsa:2048 -nodes -keyout expired.key -out expired.csr -subj "/CN=Old" 2>/dev/null
openssl ca -batch -config ca.cnf -in expired.csr -out expired.pem -notext -startdate 20200101000000Z -enddate 20200102000000Z 2>/dev/null
rm -rf expired.csr ca.cnf ca.db

openssl pkcs12 -export -in alice.pem -inkey alice.key -certfile ca.pem -out alice.p12 -passout pass:alice-p12 -name "Alice"
openssl pkcs12 -export -legacy -in alice.pem -inkey alice.key -out alice-legacy.p12 -passout pass:alice-p12 -name "Alice" 2>/dev/null ||
  openssl pkcs12 -export -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-RC2-40 -macalg sha1 -in alice.pem -inkey alice.key -out alice-legacy.p12 -passout pass:alice-p12

body() {
  printf 'Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 7bit\r\n\r\nThe launch code is %s.\r\n' "$1"
}
hdr() {
  printf 'From: %s\r\nTo: Alice <alice@example.test>\r\nSubject: %s\r\nDate: Mon, 19 Oct 2026 09:00:00 +0000\r\nMessage-ID: <%s@example.test>\r\n' "$1" "$2" "$3"
}
{ hdr "Bob <bob@example.test>" "Signed detached" signed-detached; body 1234 | openssl cms -sign -signer bob.pem -inkey bob.key -certfile ca.pem; } >signed-detached.eml
{ hdr "Bob <bob@example.test>" "Signed opaque" signed-opaque; body 2345 | openssl cms -sign -nodetach -signer bob.pem -inkey bob.key; } >signed-opaque.eml
{ hdr "Bob <bob@example.test>" "Encrypted AES" encrypted-aes; body 3456 | openssl cms -encrypt -aes-256-cbc alice.pem; } >encrypted-aes.eml
{ hdr "Bob <bob@example.test>" "Encrypted 3DES" encrypted-3des; body 4567 | openssl cms -encrypt -des3 alice.pem; } >encrypted-3des.eml
{ hdr "Bob <bob@example.test>" "Encrypted OAEP" encrypted-oaep; body 5678 | openssl cms -encrypt -aes-128-cbc -recip alice.pem -keyopt rsa_padding_mode:oaep; } >encrypted-oaep.eml
{ hdr "Bob <bob@example.test>" "Signed and encrypted" signed-encrypted
  body 6789 | openssl cms -sign -signer bob.pem -inkey bob.key | openssl cms -encrypt -aes-256-cbc alice.pem; } >signed-encrypted.eml
{ hdr "Bob <bob@example.test>" "Encrypted BER" encrypted-ber; body 8901 | openssl cms -encrypt -stream -aes-256-cbc alice.pem; } >encrypted-ber.eml
{ hdr "Carol <carol@example.test>" "Signed EC" signed-ec; body 7890 | openssl cms -sign -signer carol-ec.pem -inkey carol-ec.key -md sha384; } >signed-ec.eml
rm -f ca.key ca.srl
echo "fixtures written to $(pwd)"
