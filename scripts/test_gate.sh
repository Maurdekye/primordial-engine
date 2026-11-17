#!/usr/bin/env bash
# Self-test of the landing gate (scripts/gate.sh, scripts/audit_native.py, .gitleaks.toml, the
# reference-transaction hook and its trampoline). In a throwaway clone it plants one secret, one native
# dependency or one change to the gate per case on top of <rev>, and checks that the gate refuses each one,
# names it, and passes a clean change. Each case runs the gate as the hook does: <rev>'s scripts/gate.sh,
# with <rev> as the base (one case runs the landing's own copy instead, to check that it hands over).
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
git -C "$R" show "$sha:scripts/gate.sh" > "$tmp/gate-base.sh" || die "$rev has no scripts/gate.sh"
# A made-up publication term (built from parts, so this file doesn't hold it), and an empty allowlist, for
# every case.
term="qz$(printf -- '-%s-' planted)term"
printf 'fixed %s\n' "$term" > "$tmp/terms"
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

# -f: a case may leave the checkout dirty (case 23's case twin shares README.md's file on Windows).
new_case() { g checkout -q -f --detach "$sha" && g clean -qfdx -e .tools > /dev/null; }
# A small native executable to plant (copied with cat: MSYS cp would add .exe to the copy's name).
binary=$(type -P true); [ -f "$binary.exe" ] && binary="$binary.exe"
commit() { g add -A && g commit -q "$@"; }
relock() {  # resolve offline after planting a crate, as a lander's build would
  ${wrap[@]+"${wrap[@]}"} cargo metadata --format-version 1 --offline --manifest-path "$1/Cargo.toml" > /dev/null 2> "$tmp/relock.err" ||
    { cat "$tmp/relock.err"; die "cargo could not resolve $1 offline"; }
}
member() {  # add a crate to the root workspace
  sed -i "s|^    \"crates/pe-physics\",\$|    \"crates/pe-physics\",\n    \"$1\",|" "$R/Cargo.toml"
  grep -q "\"$1\"" "$R/Cargo.toml" || die "could not add $1 to the workspace"
}
plant_c_crate() {  # a crate of the root workspace that compiles C (cc is in the lockfile, through blake3)
  mkdir -p "$R/crates/$1/src"
  printf '[package]\nname = "%s"\nversion = "0.0.1"\nedition = "2024"\n\n[build-dependencies]\ncc = "1"\n' "$1" > "$R/crates/$1/Cargo.toml"
  echo 'fn main() { cc::Build::new().file("native.c").compile("native"); }' > "$R/crates/$1/build.rs"
  echo 'int planted(void) { return 1; }' > "$R/crates/$1/native.c"
  echo 'pub fn f() {}' > "$R/crates/$1/src/lib.rs"
  member "crates/$1"
}

pass=0 bad=0
expect() {  # name, pass|fail|error, then the regexes its gate output must match. HOW=self runs the landing's own
            # copy; BASE=<commit> lands on that commit instead of <rev> (its gate code included).
  local name=$1 want=$2; shift 2
  local log="$tmp/$name.log" rc ok=1 b=${BASE:-$sha}
  if [ "${HOW:-}" = self ]; then
    (cd "$R" && bash scripts/gate.sh --base "$b" HEAD) > "$log" 2>&1
  else
    git -C "$R" show "$b:scripts/gate.sh" > "$tmp/gate-b.sh" || die "$b has no scripts/gate.sh"
    (cd "$R" && PE_GATE_CODE=$b bash "$tmp/gate-b.sh" --base "$b" HEAD) > "$log" 2>&1
  fi
  rc=$?
  case $want in pass) [ $rc = 0 ] || ok=0 ;; fail) [ $rc = 1 ] || ok=0 ;; error) [ $rc = 2 ] || ok=0 ;; esac
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

# 7. A new crate of the workspace that compiles C.
new_case; plant_c_crate planted-native; relock "$R"; commit -m "planted native crate"
expect native-own-crate fail "planted-native 0.0.1: compiles-native \(own crate\)" "native audit FAIL"

# 8. A new crate that builds blake3 without `pure`: the workspace as a whole still has `pure` (pe-physics sets
#    it), so only the check of the crate built on its own catches it.
new_case
mkdir -p "$R/crates/planted-blake3/src"
printf '[package]\nname = "planted-blake3"\nversion = "0.0.1"\nedition = "2024"\n\n[dependencies]\nblake3 = { version = "1", default-features = false }\n' > "$R/crates/planted-blake3/Cargo.toml"
echo 'pub fn h(b: &[u8]) -> [u8; 32] { *blake3::hash(b).as_bytes() }' > "$R/crates/planted-blake3/src/lib.rs"
member crates/planted-blake3; relock "$R"; commit -m "planted blake3 user"
expect blake3-without-pure fail "blake3 1\.[0-9.]+: built by planted-blake3 built on its own.*needs feature\(s\) pure"

# 9. A crate in no workspace.
new_case
mkdir -p "$R/tools/orphan/src"
printf '[package]\nname = "orphan"\nversion = "0.0.1"\nedition = "2024"\n' > "$R/tools/orphan/Cargo.toml"
echo 'pub fn f() {}' > "$R/tools/orphan/src/lib.rs"
commit -m "orphan"
expect orphan-crate fail "tools/orphan/Cargo.toml: a crate in no workspace the gate audits"

# 10. A new workspace (as a rewrite would add) whose crate compiles C: found without any configuration.
plant_ws() {  # folder: a workspace there with one crate, c, that compiles C
  mkdir -p "$R/$1/c/src"
  printf '[workspace]\nmembers = ["c"]\nresolver = "3"\n' > "$R/$1/Cargo.toml"
  printf '[package]\nname = "c"\nversion = "0.0.1"\nedition = "2024"\n\n[build-dependencies]\ncc = "1"\n' > "$R/$1/c/Cargo.toml"
  echo 'fn main() { cc::Build::new().file("x.c").compile("x"); }' > "$R/$1/c/build.rs"
  echo 'int x(void) { return 0; }' > "$R/$1/c/x.c"
  echo 'pub fn f() {}' > "$R/$1/c/src/lib.rs"
  relock "$R/$1"
}
new_case; plant_ws lab/newws; commit -m "new workspace"
expect new-workspace-native fail "lab/newws \[x86_64-pc-windows-msvc\]: c 0.0.1: compiles-native \(own crate\)"

# 11. A publication term in the tree, and one only in the history of the new commits.
new_case; echo "see $term" >> "$R/README.md"; commit -m "term in the tree"
expect term-in-tree fail "publication check of the tree: FAIL"
new_case; echo "see $term" > "$R/t.md"; commit -m "add"; g rm -q t.md; commit -m "remove"
expect term-in-history fail "publication check of the tree: PASS" "publication check of the new commits: FAIL"

# 12. Paths a committed .gitattributes marks export-ignore are checked all the same (the export applies no
#     attributes): a new workspace whose crate compiles C, and a secret, both under such a path.
new_case; plant_ws hidden/ws; printf 'aws_access_key_id = %s\n' "$(fake aws)" > "$R/hidden/creds.txt"
printf 'hidden/** export-ignore\n' >> "$R/.gitattributes"; commit -m "hidden"
expect export-ignore-checked fail "hidden/ws \[x86_64-pc-windows-msvc\]: c 0.0.1: compiles-native \(own crate\)" "tree: aws-access-token at hidden/creds.txt"

# 13. Nor can a committed .gitattributes hide a line from the history checks: a secret and a term, each in a
#     file marked -diff, added and removed again.
new_case; printf 's.md -diff\nt.md -diff\n' >> "$R/.gitattributes"
printf 'token %s\n' "$(fake ghp)" > "$R/s.md"; echo "see $term" > "$R/t.md"; commit -m "add"; g rm -q s.md t.md; commit -m "remove"
expect diff-attribute fail "commits: github-pat at s.md" "publication check of the new commits: FAIL"

# 14. A secret that only a merge commit adds, deleted again before <rev>: found in the merge.
new_case; echo "main side" > "$R/m1.txt"; commit -m "main side"; mainside=$(git -C "$R" rev-parse HEAD)
g checkout -q --detach "$sha"; echo "side" > "$R/s1.txt"; commit -m "side"; side=$(git -C "$R" rev-parse HEAD)
g checkout -q --detach "$mainside"; g merge -q --no-ff --no-commit "$side" > /dev/null 2>&1
printf 'token %s\n' "$(fake ghp)" > "$R/evil.txt"; commit -m "merge side"; g rm -q evil.txt; commit -m "remove"
expect merge-secret fail "commits: github-pat at evil.txt" "secret scan of the tree: no leaks"

# 15. A committed .gitleaksignore counts for nothing: it fails the gate, and the secret it names is still found.
new_case; printf 'token %s\n' "$(fake ghp)" > "$R/leak2.md"; commit -m "add"; c1=$(git -C "$R" rev-parse HEAD)
printf '%s:leak2.md:github-pat:1\nleak2.md:github-pat:1\n' "$c1" > "$R/.gitleaksignore"; commit -m "ignore it"
expect gitleaksignore-refused fail "\.gitleaksignore: FAIL: the gate honours no \.gitleaksignore" "commits: github-pat at leak2.md" "tree: github-pat at leak2.md"
# An uncommitted one, in the folder the gate runs from, counts for nothing either: it names the findings of a
# secret in a file and of one in the commit's message (gitleaks reads one in the working folder for the message
# scan, whatever -i says).
new_case; printf 'token %s\n' "$(fake ghp)" > "$R/leak3.md"; commit -m "deploy with key $(fake aws)"
c1=$(git -C "$R" rev-parse HEAD)
printf '%s:leak3.md:github-pat:1\nleak3.md:github-pat:1\n:aws-access-token:3\n' "$c1" > "$R/.gitleaksignore"
expect ignore-file-in-working-folder fail "commits: github-pat at leak3.md" "tree: github-pat at leak3.md" "messages: aws-access-token"

# 16. A session.json key that isn't one of the recorded values is scanned like any other line.
new_case; mkdir -p "$R/spikes/out/planted"
printf '{\n  "members": [\n    {\n      "id": 1,\n      "key": "%s"\n    }\n  ]\n}\n' "$(fake hex)" > "$R/spikes/out/planted/session.json"
commit -m "a new session file"
expect session-key-new fail "tree: generic-api-key at spikes/out/planted/session.json"

# 17. A landing that changes the gate's code is judged by the base's: here its gate.sh and audit_native.py
#     pass everything and its .gitleaks.toml turns the default rules off, and its secret and C crate still fail.
new_case
sed -i '0,/^set -u$/s//set -u\necho "gate: PASS (planted)"; exit 0/' "$R/scripts/gate.sh"
sed -i 's/^def main():$/def main():\n    return 0\n\n\ndef unused_main():/' "$R/scripts/audit_native.py"
sed -i 's/^useDefault = true$/useDefault = false/' "$R/.gitleaks.toml"
grep -q 'planted' "$R/scripts/gate.sh" && grep -q 'unused_main' "$R/scripts/audit_native.py" &&
  grep -q 'useDefault = false' "$R/.gitleaks.toml" || die "could not plant the gate changes"
printf 'token %s\n' "$(fake ghp)" > "$R/k2.md"; plant_c_crate planted-native; relock "$R"; commit -m "a gate that passes everything"
expect gate-judged-by-base fail "NOTE: this landing changes the gate's code; it is judged by the base's" "^gate:   scripts/gate\.sh$" \
  "github-pat at k2.md" "planted-native 0.0.1: compiles-native \(own crate\)" "^gate: FAIL"

# 18. Run as the landing's own scripts/gate.sh, the gate hands over to the base's copy at once: here the
#     landing's copy never fails (a bug), and its secret fails all the same.
new_case; sed -i 's/^if \[ "\$fail" != 0 \]; then$/if false; then/' "$R/scripts/gate.sh"
grep -q '^if false; then$' "$R/scripts/gate.sh" || die "could not plant the gate.sh bug"
printf 'token %s\n' "$(fake ghp)" > "$R/k3.md"; commit -m "a gate that never fails"
HOW=self expect handover fail "with the gate's code as of ${sha:0:10}" "github-pat at k3.md" "^gate: FAIL"

# 19. The landing's own [[allowlists]] entries apply to it: a planted secret, with an exact-line entry for it.
new_case; k=$(fake ghp); printf 'token %s\n' "$k" > "$R/allowed.md"
cat >> "$R/.gitleaks.toml" << EOF

[[allowlists]]
description = "test: an exception the landing adds for its own line"
targetRules = ["github-pat", "generic-api-key"]
condition = "AND"
paths = ['''^allowed\.md$''']
regexTarget = "line"
regexes = ['''^\+?token $k$''']
EOF
commit -m "a secret with its exception"
expect allowlist-from-rev pass "NOTE: this landing changes the gate's allow files" "secret scan of the tree: no leaks" "^gate: PASS"

# 20. A gitleaks committed under .tools is never run: the gate checks the SHA-256 of whichever binary it finds.
new_case; mkdir -p "$R/.tools/gitleaks-$want"; cat "$binary" > "$R/.tools/gitleaks-$want/$exe"; chmod +x "$R/.tools/gitleaks-$want/$exe"
g add -f .tools; g commit -q -m "a gitleaks of our own"
PE_GITLEAKS= expect fake-gitleaks error "is not the pinned gitleaks $want build"

# 21. Native code in forms the audit used to miss: a build script whose module emits rustc-link-arg, and a crate
#     that ships an executable without an extension (as protoc-bin-vendored does) and has a build script.
new_case
mkdir -p "$R/crates/planted-linkarg/src" "$R/crates/planted-tool/src"
printf '[package]\nname = "planted-linkarg"\nversion = "0.0.1"\nedition = "2024"\n' > "$R/crates/planted-linkarg/Cargo.toml"
echo 'mod helper; fn main() { helper::emit(); }' > "$R/crates/planted-linkarg/build.rs"
echo 'pub fn emit() { println!("cargo::rustc-link-arg=native.o"); }' > "$R/crates/planted-linkarg/helper.rs"
echo 'pub fn f() {}' > "$R/crates/planted-linkarg/src/lib.rs"
printf '[package]\nname = "planted-tool"\nversion = "0.0.1"\nedition = "2024"\n' > "$R/crates/planted-tool/Cargo.toml"
echo 'fn main() {}' > "$R/crates/planted-tool/build.rs"
cat "$binary" > "$R/crates/planted-tool/tool"
echo 'pub fn f() {}' > "$R/crates/planted-tool/src/lib.rs"
member crates/planted-linkarg; member crates/planted-tool; relock "$R"; commit -m "native in other forms"
expect native-other-forms fail "planted-linkarg 0.0.1: links-native \(own crate\)" "planted-tool 0.0.1: prebuilt-native \(own crate\)"

# 22. A committed .cargo/config.toml can't make the audit run a program: the audit runs with the base's cargo
#     configuration (here none). The program it names would leave a marker.
new_case; mkdir -p "$R/.cargo"; mark="$tmp/cargo-config-ran"
if [ "$win" = 1 ]; then
  printf '@echo off\r\necho ran > "%s"\r\nrustc %%*\r\n' "$(cygpath -w "$mark")" > "$R/evil.cmd"; prog=./evil.cmd
else
  printf '#!/bin/sh\necho ran > "%s"\nexec rustc "$@"\n' "$mark" > "$R/evil.sh"; chmod +x "$R/evil.sh"; prog=./evil.sh
fi
printf '[build]\nrustc = "%s"\n' "$prog" > "$R/.cargo/config.toml"; commit -m "a cargo configuration that runs a program"
expect cargo-config-from-base pass "the audit ran with the base's version of" "^gate:   \.cargo/config\.toml$" "^gate: PASS"
if [ -e "$mark" ]; then bad=$((bad + 1)); echo "test_gate: BAD  cargo-config-from-base: the committed program ran"; fi

# 23. What the export refuses: a symbolic link, a submodule, and a path that differs from another only in case.
new_case; blob=$(printf 'README.md' | git -C "$R" hash-object -w --stdin)
g update-index --add --cacheinfo "120000,$blob,link" --cacheinfo "160000,$sha,sub" --cacheinfo "100644,$blob,readme.MD"
g commit -q -m "a link, a submodule and a case twin"
expect export-refusals fail "link: a symbolic link" "sub: a submodule" "readme\.MD: differs from README\.md only in case" "^gate: FAIL"

# 24. The feature policy: a shipped crate, built on its own, turns on no forbidden feature. The policy comes from
#     the base, so these checks land on a test base whose policy names planted crates: planted-app ships, and
#     planted-lib has a forbidden feature, danger, and a mutants feature. planted-harness turns both on for its own
#     build, which is allowed: the shipped crate is checked as built on its own.
new_case
mkdir -p "$R/crates/planted-lib/src" "$R/crates/planted-app/src" "$R/crates/planted-harness/src"
printf '[package]\nname = "planted-lib"\nversion = "0.0.1"\nedition = "2024"\n\n[features]\ndanger = []\nmutants = []\n' > "$R/crates/planted-lib/Cargo.toml"
echo 'pub fn f() {}' > "$R/crates/planted-lib/src/lib.rs"
printf '[package]\nname = "planted-app"\nversion = "0.0.1"\nedition = "2024"\n\n[dependencies]\nplanted-lib = { path = "../planted-lib" }\n' > "$R/crates/planted-app/Cargo.toml"
echo 'fn main() {}' > "$R/crates/planted-app/src/main.rs"
printf '[package]\nname = "planted-harness"\nversion = "0.0.1"\nedition = "2024"\n\n[dependencies]\nplanted-lib = { path = "../planted-lib", features = ["danger", "mutants"] }\n' > "$R/crates/planted-harness/Cargo.toml"
echo 'pub fn f() {}' > "$R/crates/planted-harness/src/lib.rs"
member crates/planted-lib; member crates/planted-app; member crates/planted-harness
printf '\nshipped planted-app x86_64-pc-windows-msvc x86_64-unknown-linux-gnu -- test: a shipped crate\nforbid planted-lib danger -- test: a feature only the harness may build\n' >> "$R/scripts/feature_policy.txt"
relock "$R"; commit -m "test base: planted crates, and a policy that names them"
fbase=$(git -C "$R" rev-parse HEAD)
BASE=$fbase expect feature-policy-per-crate pass "feature policy: planted-app \(\.\) checked on 2 target" "^gate: PASS"
from_fbase() { g checkout -q -f --detach "$fbase" && g clean -qfdx -e .tools > /dev/null; }
# The shipped crate turns danger on, and the landing also drops the policy's line: judged by the base's policy.
from_fbase; sed -i 's|^planted-lib = { path = "../planted-lib" }$|planted-lib = { path = "../planted-lib", features = ["danger"] }|' "$R/crates/planted-app/Cargo.toml"
sed -i '/^forbid planted-lib danger /d' "$R/scripts/feature_policy.txt"
grep -q '"danger"' "$R/crates/planted-app/Cargo.toml" && ! grep -q 'planted-lib danger' "$R/scripts/feature_policy.txt" || die "could not plant the danger feature"
commit -m "the shipped crate turns on danger, and the policy line goes"
BASE=$fbase expect feature-policy-forbidden fail "planted-app built on its own builds planted-lib 0\.0\.1 with feature danger" \
  "^gate:   scripts/feature_policy\.txt$" "^gate: the landing's own feature policy PASS" "^gate: FAIL"
from_fbase; sed -i 's|^planted-lib = { path = "../planted-lib" }$|planted-lib = { path = "../planted-lib", features = ["mutants"] }|' "$R/crates/planted-app/Cargo.toml"
grep -q '"mutants"' "$R/crates/planted-app/Cargo.toml" || die "could not plant the mutants feature"
commit -m "the shipped crate turns on mutants"
BASE=$fbase expect feature-policy-mutants fail "planted-app built on its own builds planted-lib 0\.0\.1 with feature mutants" "^gate: FAIL"
# A landing that changes the policy must also meet its own, which judges the landings after it: here one that
# doesn't parse (planted-app gets a second shipped line), one that names a target cargo can't resolve, and one
# whose tree breaks the line it adds (the base's policy allows that feature, so the native audit passes).
from_fbase; printf 'shipped planted-app x86_64-unknown-linux-gnu -- test: a second line for the same crate\n' >> "$R/scripts/feature_policy.txt"
commit -m "a second shipped line for planted-app"
BASE=$fbase expect feature-policy-own-parse fail "planted-app is already shipped on line [0-9]+" \
  "^gate: the landing's own feature policy FAIL" "^gate: FAIL"
from_fbase; sed -i 's|^shipped planted-app x86_64-pc-windows-msvc |shipped planted-app x86_64-pc-windows-msvx |' "$R/scripts/feature_policy.txt"
grep -q '^shipped planted-app x86_64-pc-windows-msvx ' "$R/scripts/feature_policy.txt" || die "could not plant the bad target"
commit -m "the policy names a target cargo can't resolve"
BASE=$fbase expect feature-policy-own-target fail "msvx" "^gate: the landing's own feature policy FAIL" "^gate: FAIL"
from_fbase; printf 'safe = []\n' >> "$R/crates/planted-lib/Cargo.toml"
sed -i 's|^planted-lib = { path = "../planted-lib" }$|planted-lib = { path = "../planted-lib", features = ["safe"] }|' "$R/crates/planted-app/Cargo.toml"
printf 'forbid planted-lib safe -- test: a feature this tree turns on\n' >> "$R/scripts/feature_policy.txt"
grep -q '"safe"' "$R/crates/planted-app/Cargo.toml" || die "could not plant the safe feature"
commit -m "the policy forbids a feature the shipped crate turns on"
BASE=$fbase expect feature-policy-own-broken fail "planted-app built on its own builds planted-lib 0\.0\.1 with feature safe" \
  "^gate: native audit PASS" "^gate: the landing's own feature policy FAIL" "^gate: FAIL"
# A landing that deletes the policy fails: from the next landing, shipped builds' features would not be checked.
from_fbase; rm "$R/scripts/feature_policy.txt"; commit -m "the landing deletes the feature policy"
BASE=$fbase expect feature-policy-own-deleted fail "this landing deletes scripts/feature_policy\.txt" \
  "^gate: native audit PASS" "^gate: FAIL"

# 25. The hook, through its trampoline: refuses a merge into main that fails, leaves main where it was, refuses
#     an update-ref, lets a clean merge in, and runs the base's hook and gate when a landing changes both, also
#     when it comes as a forced update (git gives the hook zeros as the old value). An update that leaves main
#     where it is, or moves only HEAD, returns at once, even while the audit's wrapper stalls (as a build-slot
#     wrapper does while the slots are busy): it runs no gate, so it can't hold main's lock behind one. Deleting
#     main is refused (PE_GATE=off lets it through), pack-refs still packs main, and a move that git gives an old
#     value for, while main's tip can't be read, is refused.
new_case
# The installer waits for the gate: while main's tip has no hook (here the repository's first commit), it refuses,
# since the trampoline would refuse every update of main; once main is <rev>, it installs.
g update-ref refs/heads/main "$(git -C "$R" rev-list --max-parents=0 "$sha" | tail -1)"
if (cd "$R" && bash scripts/install_gate_hook.sh > "$tmp/install-early.log" 2>&1); then
  bad=$((bad + 1)); echo "test_gate: BAD  installer-waits-for-the-gate (it installed while main has no gate)"
elif grep -q "main has no scripts/" "$tmp/install-early.log"; then
  pass=$((pass + 1)); echo "test_gate: ok   installer-waits-for-the-gate"
else
  bad=$((bad + 1)); echo "test_gate: BAD  installer-waits-for-the-gate"; sed 's/^/    /' "$tmp/install-early.log"
fi
g checkout -q -B main "$sha"
(cd "$R" && bash scripts/install_gate_hook.sh > "$tmp/install-hook.log" 2>&1) || { cat "$tmp/install-hook.log"; die "install_gate_hook.sh failed"; }
g checkout -q -b leak "$sha"; printf 'aws_secret = %s\n' "$(fake aws)" > "$R/leak.txt"; commit -m "leak"
g checkout -q -b fine "$sha"; echo "fine" >> "$R/README.md"; commit -m "fine"
g checkout -q -b fine2 "$sha"; echo "fine 2" > "$R/fine2.txt"; commit -m "fine 2"
g checkout -q -b neuter "$sha"; printf '#!/bin/sh\ncat > /dev/null\nexit 0\n' > "$R/scripts/hooks/reference-transaction"
sed -i '0,/^set -u$/s//set -u\nexit 0/' "$R/scripts/gate.sh"; printf 'aws_secret = %s\n' "$(fake aws)" > "$R/leak3.txt"; commit -m "no gate"
g checkout -q main
hk() { git -C "$R" -c user.name="gate test" -c user.email="gate-test@example.invalid" "$@"; }
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
# pe.gate.tmpdir: the gate run directly, and the hook through its trampoline, keep their temporary files in that
# folder, not TMPDIR's. The audit's wrapper records what it sees: the three variables, and which of the
# trampoline's copy of the hook and the hook's (or the hand-over's) copy of gate.sh lie there. Afterwards it is empty.
# A relative path counts for nothing: the gate keeps TMPDIR's folder and writes nothing in the folder it names.
gt="$tmp/gate-tmp"; mkdir -p "$gt"; if [ $win = 1 ]; then gt=$(cygpath -m "$gt"); fi
cat > "$tmp/record.sh" << EOF
#!/bin/sh
{ echo "TMPDIR=\$TMPDIR TMP=\$TMP TEMP=\$TEMP"
  for f in "\$TMPDIR"/*/hook "\$TMPDIR"/*/gate.sh; do [ -f "\$f" ] && echo "\${f##*/}"; done; } > "$tmp/seen.txt"
exec "\$@"
EOF
git -C "$R" config pe.gate.tmpdir "$gt"
want="TMPDIR=$gt TMP=$gt TEMP=$gt"
tmp_bad=
(cd "$R" && PE_GATE_WRAP="sh '$tmp/record.sh'" bash scripts/gate.sh --base main fine2) > "$tmp/gate-tmp-direct.log" 2>&1; rc=$?
if [ $rc != 0 ] || [ "$(cat "$tmp/seen.txt" 2> /dev/null)" != "$want
gate.sh" ] || [ -n "$(ls -A "$gt")" ]; then
  tmp_bad="$tmp_bad
    gate.sh run directly: exit $rc, saw: $(tr '\n' ' ' < "$tmp/seen.txt" 2> /dev/null), left: $(ls -A "$gt" | tr '\n' ' ')"
fi
rm -f "$tmp/seen.txt"
git -C "$R" config pe.gate.tmpdir gate-tmp-rel; mkdir -p "$R/gate-tmp-rel"
(cd "$R" && PE_GATE_WRAP="sh '$tmp/record.sh'" bash scripts/gate.sh --base main fine2) > "$tmp/gate-tmp-rel.log" 2>&1; rc=$?
if [ $rc != 0 ] || [ ! -s "$tmp/seen.txt" ] || grep -q "gate-tmp-rel" "$tmp/seen.txt" || [ -n "$(ls -A "$R/gate-tmp-rel")" ]; then
  tmp_bad="$tmp_bad
    a relative pe.gate.tmpdir: exit $rc, saw: $(head -1 "$tmp/seen.txt" 2> /dev/null), left: $(ls -A "$R/gate-tmp-rel" | tr '\n' ' ')"
fi
rm -rf "$R/gate-tmp-rel" "$tmp/seen.txt"; git -C "$R" config pe.gate.tmpdir "$gt"
git -C "$R" config pe.gate.wrap "sh '$tmp/record.sh'"
hk merge -q --no-ff -m "land fine 2" fine2 > "$tmp/hook-tmp.log" 2>&1; rc=$?
if [ $rc != 0 ] || [ "$(cat "$tmp/seen.txt" 2> /dev/null)" != "$want
hook
gate.sh" ] || [ -n "$(ls -A "$gt")" ]; then
  tmp_bad="$tmp_bad
    the hook: exit $rc, saw: $(tr '\n' ' ' < "$tmp/seen.txt" 2> /dev/null), left: $(ls -A "$gt" | tr '\n' ' ')"
fi
if [ -n "${PE_GATE_WRAP:-}" ]; then git -C "$R" config pe.gate.wrap "$PE_GATE_WRAP"; else git -C "$R" config --unset pe.gate.wrap; fi
git -C "$R" config --unset pe.gate.tmpdir
if [ -z "$tmp_bad" ]; then
  pass=$((pass + 1)); echo "test_gate: ok   gate-tmpdir (the gate run directly, and the hook through its trampoline, keep their temporary files in pe.gate.tmpdir's folder)"
else
  bad=$((bad + 1)); echo "test_gate: BAD  gate-tmpdir:$tmp_bad"; sed 's/^/    /' "$tmp/gate-tmp-direct.log" "$tmp/hook-tmp.log" | tail -8
fi
landed=$(git -C "$R" rev-parse main)
hk merge -q --no-ff -m "land no gate" neuter > "$tmp/hook-neuter.log" 2>&1; rc=$?
if [ $rc != 0 ] && [ "$(git -C "$R" rev-parse main)" = "$landed" ] && grep -q "REFUSED: main stays at $landed" "$tmp/hook-neuter.log"; then
  hk merge --abort > /dev/null 2>&1
  pass=$((pass + 1)); echo "test_gate: ok   hook-runs-the-base-hook (a landing that empties the hook and gate.sh is refused)"
else
  bad=$((bad + 1)); echo "test_gate: BAD  hook-runs-the-base-hook (exit $rc)"; sed 's/^/    /' "$tmp/hook-neuter.log"
fi
hk update-ref refs/heads/main "$(git -C "$R" rev-parse neuter)" > "$tmp/hook-forced.log" 2>&1; rc=$?
if [ $rc != 0 ] && [ "$(git -C "$R" rev-parse main)" = "$landed" ] && grep -q "REFUSED: main stays at $landed" "$tmp/hook-forced.log"; then
  pass=$((pass + 1)); echo "test_gate: ok   hook-judges-a-forced-update-by-its-base (update-ref without an old value)"
else
  bad=$((bad + 1)); echo "test_gate: BAD  hook-judges-a-forced-update-by-its-base (exit $rc, main now $(git -C "$R" rev-parse --short main))"
  sed 's/^/    /' "$tmp/hook-forced.log"; g update-ref refs/heads/main "$landed"
fi
printf '#!/bin/sh\nsleep 40\nexit 3\n' > "$tmp/stall.sh"
git -C "$R" config pe.gate.wrap "sh '$tmp/stall.sh'"
noop_bad=
for c in "reset -q --hard HEAD" "reset -q --merge HEAD" "update-ref refs/heads/main $landed $landed" \
         "checkout -q --detach" "checkout -q main"; do
  s0=$(date +%s); timeout 120 git -C "$R" $c < /dev/null > "$tmp/hook-noop.log" 2>&1; rc=$?; s1=$(date +%s)
  locks=$(cd "$R/.git" && ls HEAD.lock refs/heads/main.lock 2> /dev/null)
  if [ $rc != 0 ] || [ $((s1 - s0)) -gt 15 ] || [ -n "$locks" ] || grep -q "running the landing gate" "$tmp/hook-noop.log"; then
    noop_bad="$noop_bad
    git $c: exit $rc in $((s1 - s0)) s${locks:+, left $locks}: $(tr '\n' ' ' < "$tmp/hook-noop.log" | cut -c1-200)"
  fi
done
if [ -n "${PE_GATE_WRAP:-}" ]; then git -C "$R" config pe.gate.wrap "$PE_GATE_WRAP"; else git -C "$R" config --unset pe.gate.wrap; fi
if [ -z "$noop_bad" ] && [ "$(git -C "$R" rev-parse main)" = "$landed" ]; then
  pass=$((pass + 1)); echo "test_gate: ok   hook-no-op-returns-at-once (reset --hard HEAD, reset --merge HEAD, update-ref to the same value, checkouts that move only HEAD; stdin /dev/null; audit wrapper stalled)"
else
  bad=$((bad + 1)); echo "test_gate: BAD  hook-no-op-returns-at-once:$noop_bad"
fi
del_bad=
for c in "update-ref -d refs/heads/main" "update-ref -d refs/heads/main $landed" "branch -m main renamed"; do
  hk $c < /dev/null > "$tmp/hook-del.log" 2>&1; rc=$?
  if [ $rc = 0 ] || [ "$(git -C "$R" rev-parse -q --verify refs/heads/main)" != "$landed" ] ||
     ! grep -q "REFUSED: deleting main" "$tmp/hook-del.log"; then
    del_bad="$del_bad
    git $c: exit $rc: $(tr '\n' ' ' < "$tmp/hook-del.log" | cut -c1-200)"
    g update-ref refs/heads/main "$landed"; g symbolic-ref HEAD refs/heads/main; g branch -q -D renamed 2> /dev/null
  fi
done
PE_GATE=off hk update-ref -d refs/heads/main < /dev/null > "$tmp/hook-del.log" 2>&1; rc=$?
if [ $rc != 0 ] || git -C "$R" rev-parse -q --verify refs/heads/main > /dev/null || ! grep -q "UNCHECKED" "$tmp/hook-del.log"; then
  del_bad="$del_bad
    PE_GATE=off git update-ref -d refs/heads/main: exit $rc: $(tr '\n' ' ' < "$tmp/hook-del.log" | cut -c1-200)"
fi
g update-ref refs/heads/main "$landed"
if [ -z "$del_bad" ]; then
  pass=$((pass + 1)); echo "test_gate: ok   hook-refuses-deleting-main (update-ref -d with and without an old value, branch -m; PE_GATE=off lets it through)"
else
  bad=$((bad + 1)); echo "test_gate: BAD  hook-refuses-deleting-main:$del_bad"
fi
hk pack-refs --all < /dev/null > "$tmp/hook-pack.log" 2>&1; rc=$?
if [ $rc = 0 ] && [ "$(git -C "$R" rev-parse main)" = "$landed" ] && grep -qx "$landed refs/heads/main" "$R/.git/packed-refs" &&
   [ ! -e "$R/.git/refs/heads/main" ]; then
  pass=$((pass + 1)); echo "test_gate: ok   hook-lets-pack-refs-pack-main (as git gc does: main's loose file goes, its tip stays)"
else
  bad=$((bad + 1)); echo "test_gate: BAD  hook-lets-pack-refs-pack-main (exit $rc)"; sed 's/^/    /' "$tmp/hook-pack.log"
fi
# The trampoline and the hook, run as git runs them but with a git that can't read main (which is there, packed):
# a move that git gives an old value for, and a forced update (zeros), are refused rather than judged as main's
# creation; also when grep fails on packed-refs (exit 2: it can't tell whether main is there).
mkdir -p "$tmp/shim" "$tmp/shim-grep"
printf '#!/bin/sh\ncase "$*" in "rev-parse -q --verify refs/heads/main") exit 1 ;; esac\nexec "%s" "$@"\n' "$(type -P git)" > "$tmp/shim/git"
cat "$tmp/shim/git" > "$tmp/shim-grep/git"; printf '#!/bin/sh\nexit 2\n' > "$tmp/shim-grep/grep"
chmod +x "$tmp/shim/git" "$tmp/shim-grep/git" "$tmp/shim-grep/grep"
nowhere=$(printf 'f%.0s' $(seq 1 ${#landed})) zeros=$(printf '0%.0s' $(seq 1 ${#landed}))
read_bad=
for s in shim shim-grep; do
  sp="$tmp/$s"; if [ $win = 1 ]; then sp=$(cygpath -u "$sp"); fi   # PATH splits at a drive letter's colon
  for h in "$(git -C "$R" config --get core.hooksPath)/reference-transaction" "$R/scripts/hooks/reference-transaction"; do
    for o in "$landed" "$zeros"; do
      (cd "$R" && printf '%s %s refs/heads/main\n' "$o" "$nowhere" | PATH="$sp:$PATH" sh "$h" prepared) > "$tmp/hook-read.log" 2>&1; rc=$?
      if [ $rc = 0 ] || ! grep -q "can't be read" "$tmp/hook-read.log"; then
        read_bad="$read_bad
    $s, $h, old $(printf '%s' "$o" | cut -c1-10): exit $rc: $(tr '\n' ' ' < "$tmp/hook-read.log" | cut -c1-200)"
      fi
    done
  done
done
if [ -z "$read_bad" ]; then
  pass=$((pass + 1)); echo "test_gate: ok   hook-refuses-when-main-cannot-be-read (the trampoline and the hook; a move with an old value, and a forced update; also when grep fails on packed-refs)"
else
  bad=$((bad + 1)); echo "test_gate: BAD  hook-refuses-when-main-cannot-be-read:$read_bad"
fi
t0=$(date +%s); hk branch -q other "$sha"; hk branch -q -D other; t1=$(date +%s)
echo "test_gate: note: two updates of another branch with the hook on took $((t1 - t0)) s"

echo "test_gate: $pass ok, $bad bad"
[ $bad = 0 ]
