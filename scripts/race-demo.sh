#!/usr/bin/env bash
# Fires N concurrent checkouts at the seeded "Limited Edition Desk Mat"
# (3 in stock on a fresh database) and prints the outcome of each, then the
# remaining stock. Expect exactly as many 201s as there was stock, the rest
# 409 INSUFFICIENT_INVENTORY, and stock 0 -- never negative.
#
# Usage: scripts/race-demo.sh [N]   (service running on $BASE, default :8080)
set -euo pipefail
BASE=${BASE:-http://localhost:8080}
N=${1:-10}
MAT=00000000-0000-4000-8000-000000000005

json_field() { python3 -c "import sys, json; print(json.load(sys.stdin)$1)"; }

echo "stock before: $(curl -s "$BASE/products/$MAT" | json_field "['stock']")"
carts=()
for _ in $(seq "$N"); do
  cart=$(curl -s -X POST "$BASE/carts" | json_field "['id']")
  curl -s -o /dev/null -X POST "$BASE/carts/$cart/items" \
    -H 'content-type: application/json' -d "{\"product_id\":\"$MAT\",\"quantity\":1}"
  carts+=("$cart")
done

printf '%s\n' "${carts[@]}" | xargs -P "$N" -I{} \
  curl -s -o /dev/null -w '%{http_code}\n' -X POST "$BASE/carts/{}/checkout" -H 'Idempotency-Key: race-{}' \
  | sort | uniq -c | sed 's/^/  HTTP /'

echo "stock after:  $(curl -s "$BASE/products/$MAT" | json_field "['stock']")"
