#!/bin/sh
# Writes deploy/.env for Docker Compose with fresh secrets, once.
#   ./setup.sh                                  try it on this machine (dashboard on 127.0.0.1:8788)
#   ./setup.sh https://tokenrein.example.com    the dashboard's address behind your HTTPS proxy
set -eu
cd "$(dirname "$0")"
if [ -e .env ]; then
  echo ".env already exists; edit it, or delete it to start over" >&2
  exit 1
fi
command -v openssl >/dev/null || { echo "openssl is needed to make the secrets" >&2; exit 1; }
url=${1:-http://127.0.0.1:8788}
pw=$(openssl rand -hex 16)
umask 077
sed -e "s|^TOKENREIN_ADMIN_TOKEN=.*|TOKENREIN_ADMIN_TOKEN=$(openssl rand -hex 24)|" \
    -e "s|^TOKENREIN_SECRETS_KEY=.*|TOKENREIN_SECRETS_KEY=$(openssl rand -hex 32)|" \
    -e "s|^POSTGRES_PASSWORD=.*|POSTGRES_PASSWORD=$pw|" \
    -e "s|<POSTGRES_PASSWORD>|$pw|" \
    -e "s|^TOKENREIN_PUBLIC_URL=.*|TOKENREIN_PUBLIC_URL=$url|" \
    .env.example > .env
echo "Wrote $(pwd)/.env with new secrets (back up TOKENREIN_SECRETS_KEY: it encrypts saved provider keys)."
echo "Next:"
echo "  docker compose up -d"
echo "  docker compose exec tokenrein tokenrein create-owner you@company.com"
