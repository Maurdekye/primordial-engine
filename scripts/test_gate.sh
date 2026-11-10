#!/usr/bin/env bash
# Self-test of the landing gate (scripts/gate.sh, scripts/audit_native.py, .gitleaks.toml and the
# reference-transaction hook). In a throwaway clone it plants one secret or one native dependency per case
# on top of <rev>, and checks that the gate refuses each one, names it, and passes a clean change.
#
#   bash scripts/test_gate.sh [<rev>]          (default HEAD)
#
# Needs gitleaks (bash scripts/install_gitleaks.sh, or PE_GITLEAKS) and cargo with the crates of <rev>'s
# lockfiles already downloaded: it resolves the planted crates offline. Honours PE_GATE_WRAP like the gate
# (its cargo calls run under it too). The planted secrets are made up at run time, so this file holds none.
# Exit 0 if every case behaves, 1 if one doesn't (its gate output is printed), 2 on an error.
set -u
die() { echo "test_gate: ERROR: $*" >&2; exit 2; }
rev=${1:-HEAD}
sha=$(git rev-parse -q --verify "$rev^{commit}") || die "$rev is not a commit"
common=$(git rev-parse --path-format=absolute --git-common-dir)
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) win=1 exe=gitleaks.exe ;; *) win=0 exe=gitleaks ;; esac
want=$(git show "$sha:scripts/install_gitleaks.sh" | sed -n 's/^VERSION=//p')
if [ -z "${PE_GITLEAKS:-}" ]; then
  for d in "$(git rev-parse --show-toplevel)" "$(dirname "$common")"; do
    [ -x "$d/.tools/gitleaks-$want/$exe" ] && { PE_GITLEAKS="$d/.tools/gitleaks-$want/$exe"; break; }
  done
fi
[ -n "${PE_GITLEAKS:-}" ] || die "gitleaks $want not found (bash scripts/install_gitleaks.sh)"
export PE_GITLEAKS
py=${PE_PYTHON:-}; [ -n "$py" ] || { if [ "$win" = 1 ]; then py=python; else py=python3; fi; }
wrap=(); if [ -n "${PE_GATE_WRAP:-}" ]; then eval "wrap=($PE_GATE_WRAP)"; fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git clone -q --shared --no-checkout "$common" "$tmp/repo" || die "clone failed"
R="$tmp/repo"
g() { git -C "$R" -c user.name="gate test" -c user.email="gate-test@example.invalid" -c core.hooksPath=/dev/null "$@"; }
g checkout -q --detach "$sha" || die "checkout failed"
# A made-up publication term (built from parts, so this file doesn't hold it), and an empty allowlist, for
# every case.
term="qz$(printf -- '-%s-' planted)term"
printf 'fixed %s
' "$term" > "$tmp/terms"
: > "$tmp/allow"
export PE_PUBLISH_TERMS="$tmp/terms" PE_PUBLISH_ALLOW="$tmp/allow"

# Made-up secrets, built here so that no file holds one.
fake() { "$py" -I -c "
import random, string, sys
r = random.Random()
k = sys.argv[1]
if k == 'aws': print('AKIA' + ''.join(r.choice(string.ascii_uppercase + '234567') for _ in range(16)))
elif k == 'ghp': print('ghp_' + ''.join(r.choice(string.ascii_letters + string.digits) for _ in range(36)))
elif k == 'hex': print(''.join(r.choice('0123456789abcdef') for _ in range(64)))
" "$1"; }

new_case() { g checkout -q --detach "$sha" && g clean -qfdx -e .tools > /dev/null; }
commit() { g add -A && g commit -q "$@"; }
relock() {  # resolve offline after planting a crate, as a lander's build would
  ${wrap[@]+"${wrap[@]}"} cargo metadata --format-version 1 --offline --manifest-path "$1/Cargo.toml" > /dev/null 2> "$tmp/relock.err" ||
    { cat "$tmp/relock.err"; die "cargo could not resolve $1 offline"; }
}

pass=0 bad=0
expect() {  # name, pass|fail, then the regexes its gate output must match
  local name=$1 want=$2; shift 2
  local log="$tmp/$name.log" rc ok=1
  (cd "$R" && bash scripts/gate.sh --base "$sha" HEAD) > "$log" 2>&1
  rc=$?
  if [ "$want" = pass ] && [ $rc != 0 ]; then ok=0; fi
  if [ "$want" = fail ] && [ $rc != 1 ]; then ok=0; fi
  for re in "$@"; do grep -Eq -- "$re" "$log" || { ok=0; echo "test_gate: $name: no line matches /$re/"; }; done
  if [ $ok = 1 ]; then
    pass=$((pass + 1)); echo "test_gate: ok   $name (gate exit $rc)"
  else
    bad=$((bad + 1)); echo "test_gate: BAD  $name (gate exit $rc, wanted $want)"; sed 's/^/    /' "$log"
  fi
}

# 1. A clean change passes.
new_case; echo "gate self-test" >> "$R/README.md"; commit -m "a clean change"
expect clean pass "^gate: PASS" "publication check of the new commits: no term"

# 2. A secret in the tree: found in the commit and in the tree.
new_case; mkdir -p "$R/notes"; printf 'aws_access_key_id = %s\n' "$(fake aws)" > "$R/notes/creds.txt"; commit -m "notes"
expect secret-in-tree fail "commits: aws-access-token at notes/creds.txt" "tree: aws-access-token at notes/creds.txt" "^gate: FAIL"

# 3. A secret added, then deleted: only the history has it, and the history is what gets published.
new_case; printf 'token %s\n' "$(fake ghp)" > "$R/scratch.md"; commit -m "add"; g rm -q scratch.md; commit -m "remove"
expect secret-in-history fail "commits: github-pat at scratch.md" "secret scan of the tree: no leaks"

# 4. A secret only in a commit message.
new_case; g commit -q --allow-empty -m "deploy with key $(fake aws)"
expect secret-in-message fail "messages: aws-access-token"

# 5. An inline gitleaks:allow comment does not let a secret through.
new_case; printf 'const K: &str = "%s"; // gitleaks:allow\n' "$(fake ghp)" > "$R/k.rs"; commit -m "k"
expect inline-allow-ignored fail "tree: github-pat at k.rs"

# 6. A pe-app member key file.
new_case; mkdir -p "$R/spikes/out/m1/planted"; fake hex > "$R/spikes/out/m1/planted/member-1.key"; commit -m "keys"
expect member-key-file fail "pe-member-key-file at spikes/out/m1/planted/member-1.key"

# 7. A new crate of the workspace that compiles C (cc is already in the lockfile, through blake3).
new_case
mkdir -p "$R/crates/planted-native/src"
cat > "$R/crates/planted-native/Cargo.toml" << 'EOF'
[package]
name = "planted-native"
version = "0.0.1"
edition = "2024"

[build-dependencies]
cc = "1"
EOF
echo 'fn main() { cc::Build::new().file("native.c").compile("native"); }' > "$R/crates/planted-native/build.rs"
echo 'int planted(void) { return 1; }' > "$R/crates/planted-native/native.c"
echo 'pub fn f() {}' > "$R/crates/planted-native/src/lib.rs"
sed -i 's|^    "crates/pe-app",$|    "crates/pe-app",\n    "crates/planted-native",|' "$R/Cargo.toml"
grep -q planted-native "$R/Cargo.toml" || die "could not add the planted crate to the workspace"
relock "$R"; commit -m "planted native crate"
expect native-own-crate fail "planted-native 0.0.1: compiles-native \(own crate\)" "native audit FAIL"

# 8. A new crate that builds blake3 without `pure`: the workspace as a whole still has `pure` (other crates
#    set it), so only the check of the crate built on its own catches it.
new_case
mkdir -p "$R/crates/planted-blake3/src"
cat > "$R/crates/planted-blake3/Cargo.toml" << 'EOF'
[package]
name = "planted-blake3"
version = "0.0.1"
edition = "2024"

[dependencies]
blake3 = { version = "1", default-features = false }
EOF
echo 'pub fn h(b: &[u8]) -> [u8; 32] { *blake3::hash(b).as_bytes() }' > "$R/crates/planted-blake3/src/lib.rs"
sed -i 's|^    "crates/pe-app",$|    "crates/pe-app",\n    "crates/planted-blake3",|' "$R/Cargo.toml"
relock "$R"; commit -m "planted blake3 user"
expect blake3-without-pure fail "blake3 1\.[0-9.]+: built by planted-blake3 built on its own.*needs feature\(s\) pure"

# 9. A crate in no workspace.
new_case
mkdir -p "$R/tools/orphan/src"
printf '[package]\nname = "orphan"\nversion = "0.0.1"\nedition = "2024"\n' > "$R/tools/orphan/Cargo.toml"
echo 'pub fn f() {}' > "$R/tools/orphan/src/lib.rs"
commit -m "orphan"
expect orphan-crate fail "tools/orphan/Cargo.toml: a crate in no workspace the gate audits"

# 10. A new workspace (as a rewrite would add) whose crate compiles C: found without any configuration.
new_case
mkdir -p "$R/lab/newws/c/src"
printf '[workspace]\nmembers = ["c"]\nresolver = "3"\n' > "$R/lab/newws/Cargo.toml"
printf '[package]\nname = "c"\nversion = "0.0.1"\nedition = "2024"\n\n[build-dependencies]\ncc = "1"\n' > "$R/lab/newws/c/Cargo.toml"
echo 'fn main() { cc::Build::new().file("x.c").compile("x"); }' > "$R/lab/newws/c/build.rs"
echo 'int x(void) { return 0; }' > "$R/lab/newws/c/x.c"
echo 'pub fn f() {}' > "$R/lab/newws/c/src/lib.rs"
relock "$R/lab/newws"; commit -m "new workspace"
expect new-workspace-native fail "lab/newws \[x86_64-pc-windows-msvc\]: c 0.0.1: compiles-native \(own crate\)"

# 11. A publication term in the tree, and one only in the history of the new commits.
new_case; echo "see $term" >> "$R/README.md"; commit -m "term in the tree"
expect term-in-tree fail "publication check of the tree: FAIL"
new_case; echo "see $term" > "$R/t.md"; commit -m "add"; g rm -q t.md; commit -m "remove"
expect term-in-history fail "publication check of the tree: PASS" "publication check of the new commits: FAIL"

# 12. The hook: refuses a merge into main that fails, leaves main where it was, and lets a clean one in.
new_case
g checkout -q -B main "$sha"
g checkout -q -b leak "$sha"; printf 'aws_secret = %s\n' "$(fake aws)" > "$R/leak.txt"; commit -m "leak"
g checkout -q -b fine "$sha"; echo "fine" >> "$R/README.md"; commit -m "fine"
g checkout -q main
hk() { git -C "$R" -c user.name="gate test" -c user.email="gate-test@example.invalid" -c core.hooksPath="$R/scripts/hooks" "$@"; }
[ -n "${PE_GATE_WRAP:-}" ] && git -C "$R" config pe.gate.wrap "$PE_GATE_WRAP"
hk merge -q --no-ff -m "land leak" leak > "$tmp/hook-leak.log" 2>&1; rc=$?
after=$(git -C "$R" rev-parse main)
if [ $rc != 0 ] && [ "$after" = "$sha" ] && grep -q "REFUSED: main stays at $sha" "$tmp/hook-leak.log"; then
  hk merge --abort > /dev/null 2>&1
  if [ -z "$(git -C "$R" status --porcelain --untracked-files=no)" ]; then
    pass=$((pass + 1)); echo "test_gate: ok   hook-refuses-merge (main unchanged; git merge --abort restores the checkout)"
  else
    bad=$((bad + 1)); echo "test_gate: BAD  hook-refuses-merge: git merge --abort left changes"; git -C "$R" status --short | head
  fi
else
  bad=$((bad + 1)); echo "test_gate: BAD  hook-refuses-merge (exit $rc, main now $after)"; sed 's/^/    /' "$tmp/hook-leak.log"
fi
hk update-ref refs/heads/main "$(git -C "$R" rev-parse leak)" > "$tmp/hook-ur.log" 2>&1; rc=$?
if [ $rc != 0 ] && [ "$(git -C "$R" rev-parse main)" = "$sha" ]; then
  pass=$((pass + 1)); echo "test_gate: ok   hook-refuses-update-ref"
else
  bad=$((bad + 1)); echo "test_gate: BAD  hook-refuses-update-ref (exit $rc)"; sed 's/^/    /' "$tmp/hook-ur.log"
fi
hk merge -q --no-ff -m "land fine" fine > "$tmp/hook-fine.log" 2>&1; rc=$?
if [ $rc = 0 ] && [ "$(git -C "$R" rev-parse main^2)" = "$(git -C "$R" rev-parse fine)" ]; then
  pass=$((pass + 1)); echo "test_gate: ok   hook-lets-clean-merge-in"
else
  bad=$((bad + 1)); echo "test_gate: BAD  hook-lets-clean-merge-in (exit $rc)"; sed 's/^/    /' "$tmp/hook-fine.log"
fi
t0=$(date +%s); hk branch -q other "$sha"; hk branch -q -D other; t1=$(date +%s)
echo "test_gate: note: two updates of another branch with the hook on took $((t1 - t0)) s"

echo "test_gate: $pass ok, $bad bad"
[ $bad = 0 ]
