#!/usr/bin/env bash
# Signs gz in with a Groundzero user, non-interactively: the password (and
# the authenticator code, when the user has MFA) go to `gz auth login` on
# stdin, never on the command line.
#
# Env: GZ_USERNAME, GZ_PASSWORD, GZ_MFA_SECRET (optional base32 TOTP
#      secret), and GZ_CLIENT / GZ_SITE / GZ_DOMAIN for where to sign in.
set -euo pipefail

fail() { echo "::error title=gz sign-in::$*" >&2; exit 1; }
[ -n "${GZ_CLIENT:-}" ] || fail "the client input is empty (the client name in https://<site>.<client>.<domain>)"
[ -n "${GZ_SITE:-}" ] || fail "the site input is empty"
[ -n "${GZ_USERNAME:-}" ] || fail "the username input is empty; pass it from a secret"
[ -n "${GZ_PASSWORD:-}" ] || fail "the password input is empty; pass it from a secret"

# The current 6-digit code for a base32 TOTP secret (RFC 6238, 30 s, SHA-1),
# the authenticator app's code.
totp() {
  local python=python3
  command -v python3 >/dev/null 2>&1 || python=python
  "$python" -I -c '
import base64, hmac, os, struct, time
secret = os.environ["GZ_MFA_SECRET"].replace(" ", "").upper()
key = base64.b32decode(secret + "=" * (-len(secret) % 8))
digest = hmac.new(key, struct.pack(">Q", int(time.time()) // 30), "sha1").digest()
offset = digest[-1] & 15
print("%06d" % ((struct.unpack(">I", digest[offset:offset + 4])[0] & 0x7FFFFFFF) % 1000000))
'
}

answers="$GZ_PASSWORD"$'\n'
if [ -n "${GZ_MFA_SECRET:-}" ]; then
  answers+="$(totp)"$'\n'
fi
printf '%s' "$answers" | gz auth login --username "$GZ_USERNAME" --password-stdin
