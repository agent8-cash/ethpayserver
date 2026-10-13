#!/usr/bin/env bash
# Proves audit-build-graph.sh blocks on an advisory in a crate we build, and
# does not on one that only the lockfile mentions. A gate that only ever passed
# proves nothing.
set -uo pipefail
GUARD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/audit-build-graph.sh"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
mkdir "$TMP/bin"
# A fake cargo: `cargo tree -i <spec>` prints a tree only for crates in $BUILT.
cat > "$TMP/bin/cargo" <<'EOS'
#!/usr/bin/env bash
spec="$3"
case " $BUILT " in *" ${spec%@*} "*) echo "${spec%@*} v1";; esac
[ "${TREE_ERR:-}" = 1 ] && exit 101
exit 0
EOS
chmod +x "$TMP/bin/cargo"
fail=0
vuln() { printf '{"vulnerabilities":{"list":[%s]}}' "$1" > "$TMP/audit.json"; }
entry() { printf '{"advisory":{"id":"%s"},"package":{"name":"%s","version":"1.0.0"}}' "$1" "$2"; }
check() { # name want-rc
  PATH="$TMP/bin:$PATH" AUDIT_JSON_FILE="$TMP/audit.json" "$GUARD" >"$TMP/out" 2>&1
  got=$?
  if [ "$got" -ne "$2" ]; then echo "FAIL: $1 - expected exit $2, got $got"; cat "$TMP/out"; fail=1; else echo "ok: $1 (exit $got)"; fi
}

export BUILT="shipped"
vuln ""; check "no advisories passes" 0
vuln "$(entry RUSTSEC-X quinn-proto)"; check "advisory in a lockfile-only crate does not block" 0
grep -q "LOCKFILE ONLY" "$TMP/out" || { echo "FAIL: lockfile-only case is not labelled in the output"; fail=1; }
vuln "$(entry RUSTSEC-Y shipped)"; check "advisory in a crate we build blocks" 1
grep -q "IN BUILD GRAPH" "$TMP/out" || { echo "FAIL: build-graph case is not labelled in the output"; fail=1; }
vuln "$(entry RUSTSEC-X quinn-proto),$(entry RUSTSEC-Y shipped)"; check "one built crate among lockfile-only ones blocks" 1
TREE_ERR=1 vuln "$(entry RUSTSEC-X quinn-proto)"; TREE_ERR=1 check "a cargo tree error fails closed" 1
echo 'not json' > "$TMP/audit.json"; check "unparseable audit output is 'could not run', not a pass" 2
exit $fail
