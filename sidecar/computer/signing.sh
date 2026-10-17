#!/bin/zsh
# One-time: a local self-signed code-signing identity, so macOS keeps the helper's
# Accessibility / Screen Recording grants across rebuilds (ad-hoc signatures change every
# build). Stored in the login keychain; only /usr/bin/codesign may use the key.
# Remove: Keychain Access → login → "Sam Local Code Signing" → delete. Builds also accept the
# identity made before the rename ("Rowan Local Code Signing"), so existing grants stay valid.
set -e
NAME="Sam Local Code Signing"
security find-identity -p codesigning 2>/dev/null | grep -qE "(Sam|Rowan) Local Code Signing" && { echo "exists"; exit 0; }
T=$(mktemp -d)
cat > $T/cfg <<CFG
[req]
distinguished_name=dn
x509_extensions=ext
prompt=no
[dn]
CN=$NAME
[ext]
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
basicConstraints=critical,CA:false
CFG
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -keyout $T/k.pem -out $T/c.pem -config $T/cfg 2>/dev/null
P=$(openssl rand -hex 12)
openssl pkcs12 -export -legacy -inkey $T/k.pem -in $T/c.pem -out $T/id.p12 -passout pass:$P 2>/dev/null || openssl pkcs12 -export -inkey $T/k.pem -in $T/c.pem -out $T/id.p12 -passout pass:$P
security import $T/id.p12 -k ~/Library/Keychains/login.keychain-db -P $P -T /usr/bin/codesign >/dev/null
rm -rf $T
echo "created $NAME"
