#!/usr/bin/env bash
# Tests for publish_check.sh's allowlist (scripts/publish_allow.py), in a throwaway repo with a made-up
# term. Exits 1 on the first failed case. usage: bash scripts/test_publish_allow.sh
set -u
here="$(cd "$(dirname "$0")" && pwd)"
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
fail() { echo "test_publish_allow: FAIL: $*"; exit 1; }
cd "$t" && git init -q && mkdir scripts
cp "$here/publish_check.sh" "$here/publish_allow.py" scripts/
printf 'word zqxtermq\n' > scripts/publish_terms.local
printf 'ok line\nvalue zqxtermq here\n' > a.txt
printf 'bin\0zqxtermq\0' > b.bin
printf 'zqxtermq again\n' > c.txt
git add -A && git -c user.name=t -c user.email=t@t commit -qm t
python -I scripts/publish_allow.py hash a.txt 2 > scripts/publish_allow.local
check() { PE_PUBLISH_ALLOW="${1:-scripts/publish_allow.local}" bash scripts/publish_check.sh > out.txt; echo $?; }

# A binary match and a text match after an allowlisted line: both still fail (B1, 2026-11-07).
[ "$(check)" = 1 ] || fail "binary + text after an allowlisted line did not fail"
grep -q '^Binary file HEAD:b.bin matches$' out.txt || fail "the binary match is not reported"
grep -q '^allowed: HEAD:a.txt:2 ' out.txt || fail "the allowlisted line is not reported"
grep -q 'FAIL, 2 line(s) .* (1 allowlisted)' out.txt || fail "wrong counts: $(tail -1 out.txt)"
# Without the list: all three.
[ "$(check /nonexistent)" = 1 ] && grep -q 'FAIL, 3 line(s)' out.txt || fail "without the list, not 3 hits"
# A broken allowlist makes the filter fail: the check fails closed (exit 2).
printf 'zz notahash a.txt\n' > bad.txt
[ "$(check bad.txt)" = 2 ] || fail "a failing filter did not exit 2"
# An edited line no longer matches its entry.
printf 'ok line\nvalue zqxtermq here!\n' > a.txt && git rm -q b.bin c.txt && git -c user.name=t -c user.email=t@t commit -qam t2
[ "$(check)" = 1 ] || fail "an edited allowlisted line passed"
# Only the listed line, unedited: passes, and still says so.
printf 'ok line\nvalue zqxtermq here\n' > a.txt && git -c user.name=t -c user.email=t@t commit -qam t3
[ "$(check)" = 0 ] && grep -q 'PASS, .* except 1 allowlisted' out.txt || fail "the allowlisted line alone did not pass"
echo "test_publish_allow: PASS (6 cases)"
