#!/usr/bin/env bash
# Executar apenas contra uma raiz própria; cria e remove uma coleção temporária.
set -euo pipefail
: "${DAV_URL:?Defina DAV_URL=https://seu-dominio}"
: "${DAV_USER:?Defina DAV_USER}"
[[ "$DAV_URL" == https://* && "$DAV_URL" != *'?'* && "$DAV_URL" != *'#'* ]] || { echo 'URL HTTPS sem query/fragmento obrigatória' >&2; exit 1; }
# curl solicita a senha no terminal; arquivo temporário evita senha em argv/logs.
work=$(mktemp -d)
chmod 700 "$work"
trap 'rm -rf "$work"' EXIT
read -rsp 'Senha WebDAV: ' password; printf '\n'
# Sintaxe curl config: escapar barras e aspas, rejeitar quebras de linha.
[[ "$password$DAV_USER" != *$'\n'* && "$password$DAV_USER" != *$'\r'* && "$DAV_USER" != *:* ]] || exit 1
credential="$DAV_USER:$password"
credential=${credential//\\/\\\\}; credential=${credential//\"/\\\"}
printf 'user = "%s"\n' "$credential" > "$work/auth"
unset password credential
base="${DAV_URL%/}/preflight-$(cat /proc/sys/kernel/random/uuid)"
request() { curl --connect-timeout 15 --max-time 120 --silent --show-error --config "$work/auth" --output "$work/body" --write-out '%{http_code}' "$@"; }
expect() { local want=$1; shift; local got; got=$(request "$@"); [[ "$got" == "$want" ]] || { echo "Esperado HTTP $want; recebido $got" >&2; exit 1; }; }
cleanup() { request -X DELETE "$base/" >/dev/null || true; rm -rf "$work"; }
expect 201 -X MKCOL "$base/"
trap cleanup EXIT
expect 207 -X PROPFIND -H 'Depth: 1' "$base/"
expect 201 -X PUT --data-binary 'primeiro' "$base/first.tmp"
expect 201 -X MOVE -H "Destination: $base/final" -H 'Overwrite: F' "$base/first.tmp"
expect 201 -X PUT --data-binary 'segundo' "$base/second.tmp"
expect 412 -X MOVE -H "Destination: $base/final" -H 'Overwrite: F' "$base/second.tmp"
expect 200 "$base/final"
[[ $(cat "$work/body") == primeiro ]] || { echo 'Conteúdo original alterado' >&2; exit 1; }
expect 200 "$base/second.tmp"
[[ $(cat "$work/body") == segundo ]] || exit 1
expect 204 -X DELETE "$base/"
trap 'rm -rf "$work"' EXIT
echo 'PROPFIND, PUT e MOVE com Overwrite:F verificados; destino preservado.'
