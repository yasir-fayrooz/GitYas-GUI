#!/usr/bin/env bash
# One-time setup (macOS): creates the self-signed code-signing identity "GitYas Dev" in your login keychain.
#
# Why: cargo builds are signed ad hoc, so every rebuild is a different app to the keychain and macOS asks for your
# password again for each saved token. `scripts/macos-run.sh` (the cargo runner) signs `gityas` with this identity
# instead, so the keychain recognises every build as the same app: answer "Always Allow" once per saved item.
#
# The certificate is local to this Mac, only trusted for code signing, and never leaves it. macOS asks for your
# password once to trust it. Remove it any time in Keychain Access (login → My Certificates → GitYas Dev).
set -euo pipefail

NAME="GitYas Dev"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if [[ "$(uname)" != "Darwin" ]]; then
  echo "Only needed on macOS." >&2
  exit 0
fi
if security find-identity -v -p codesigning | grep -q "\"$NAME\""; then
  echo "\"$NAME\" already exists; nothing to do."
  exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cat >"$work/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
EOF
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$work/cert.cnf" \
  -keyout "$work/key.pem" -out "$work/cert.pem" 2>/dev/null
pass="gityas-dev-$RANDOM$RANDOM"
openssl pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" -name "$NAME" \
  -out "$work/identity.p12" -passout "pass:$pass"
# -T: codesign may use the key without asking.
security import "$work/identity.p12" -k "$KEYCHAIN" -P "$pass" -T /usr/bin/codesign
echo "Trusting \"$NAME\" for code signing (macOS asks for your password)…"
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$work/cert.pem"

if security find-identity -v -p codesigning | grep -q "\"$NAME\""; then
  echo "Done. \`cargo run -p gityas-frontend\` now signs the app as \"$NAME\"."
  echo "On the next launch, answer \"Always Allow\" once for each GitYas keychain item."
else
  echo "The certificate was imported but is not a valid signing identity; check Keychain Access." >&2
  exit 1
fi
