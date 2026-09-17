#!/bin/sh
#
# Put the Space's OAuth client id in the page, then serve it.
#
# `OAUTH_CLIENT_ID` is what `hf_oauth: true` in the README puts in this container's environment. It
# is public — it identifies the app and authorises nothing — which is why it can be baked into a
# page anybody can read. `OAUTH_CLIENT_SECRET` is in this environment too and must never go near
# the page: the browser flow is PKCE and needs no secret.
set -eu

if [ -z "${OAUTH_CLIENT_ID:-}" ]; then
    # Not fatal. The page says so itself and offers `?client_id=`, and a playground that serves
    # and explains beats one that will not start. The likeliest cause is `hf_oauth: true` missing.
    echo "no OAUTH_CLIENT_ID in the environment; nobody will be able to sign in" >&2
fi

mkdir -p /srv
sed "s|{{OAUTH_CLIENT_ID}}|${OAUTH_CLIENT_ID:-}|g" /app/index.html > /srv/index.html

echo "serving the playground on 7860 (oauth client ${OAUTH_CLIENT_ID:-none})"
exec python3 -m http.server 7860 --directory /srv
