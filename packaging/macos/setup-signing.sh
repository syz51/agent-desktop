#!/usr/bin/env bash
set -euo pipefail

# Creates a persistent local self-signed Code Signing certificate in the user's login Keychain
# so macOS TCC grants (Accessibility + Screen Recording) for AgentDesktop.app survive every
# Cargo rebuild without needing an Apple Developer ID account.

CERT_NAME="${AGENT_DESKTOP_CERT_NAME:-AgentDesktop Local Signer}"
KEYCHAIN="${AGENT_DESKTOP_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"

if security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null | grep -Fq "\"$CERT_NAME\""; then
    echo "Code signing identity '$CERT_NAME' already exists in $KEYCHAIN."
    security find-identity -v -p codesigning "$KEYCHAIN" | grep -F "\"$CERT_NAME\""
    exit 0
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/agent-desktop-cert.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT

cat > "$TMP_DIR/cert.cnf" <<EOF
[ req ]
default_bits       = 2048
distinguished_name = req_dn
x509_extensions    = v3_codesign
prompt             = no

[ req_dn ]
CN = $CERT_NAME

[ v3_codesign ]
basicConstraints       = critical, CA:FALSE
keyUsage               = critical, digitalSignature
extendedKeyUsage       = critical, codeSigning
subjectKeyIdentifier   = hash
EOF

openssl req -x509 -newkey rsa:2048 -nodes \
    -days 3650 \
    -config "$TMP_DIR/cert.cnf" \
    -keyout "$TMP_DIR/key.pem" \
    -out "$TMP_DIR/cert.pem"

# Export PKCS#12 using legacy-compatible encryption supported by macOS Keychain `security import`.
# OpenSSL 3+/4+ defaults to PBES2/SHA-256 MAC which macOS SecKeychainItemImport rejects unless `-legacy` is set.
P12_PASS="agent-desktop-local-import"
PKCS12_EXTRA=()
if openssl pkcs12 -help 2>&1 | grep -q -- '-legacy'; then
    PKCS12_EXTRA=(-legacy)
fi
openssl pkcs12 -export \
    "${PKCS12_EXTRA[@]}" \
    -inkey "$TMP_DIR/key.pem" \
    -in "$TMP_DIR/cert.pem" \
    -out "$TMP_DIR/cert.p12" \
    -passout "pass:$P12_PASS"

security import "$TMP_DIR/cert.p12" \
    -k "$KEYCHAIN" \
    -P "$P12_PASS" \
    -T /usr/bin/codesign \
    -T /usr/bin/security

echo "Marking '$CERT_NAME' as trusted for Code Signing in your login Keychain (may prompt once)..."
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$TMP_DIR/cert.pem"

echo "Done. Verified signing identity:"
security find-identity -v -p codesigning "$KEYCHAIN" | grep -F "\"$CERT_NAME\""
