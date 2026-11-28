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
trap '[ -z "${spid:-}" ] || kill "$spid" 2> /dev/null; rm -rf "$tmp"' EXIT
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
# Tier 2's state is checked only where case 25 asks for it, against files standing in for GitHub.
unset PE_GATE_TIER2 PE_CI_RUNS_URL PE_CI_REPO PE_GATE_GHTOKEN

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

# 11b. A mail id (built from parts, so this file holds none) in the tree, and one only in a commit message.
mid="m$(printf '%06x%06x' 11259375 1193046)"
new_case; echo "see $mid" >> "$R/README.md"; commit -m "mail id in the tree"
expect mail-id-in-tree fail "process check: FAIL" "README.md:[0-9]+: a mail id"
new_case; echo "process" >> "$R/README.md"; commit -m "after $mid"
expect mail-id-in-message fail "process check: FAIL" "a mail id in its message"
new_case; echo "x${mid}y and ${mid}0" >> "$R/README.md"; commit -m "longer words"
expect mail-id-inside-a-word pass "process check: PASS"
# 11c. A bare CR (a CR without its LF, as a join of two CRLF lines left it) in the tree.
new_case; printf 'joined,\r line\n' >> "$R/README.md"; commit -m "bare CR"
expect bare-cr-in-tree fail "process check: FAIL" "README.md:[0-9]+: a bare CR"

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

# 25. Tier 2's state (C7), with files standing in for GitHub's answers (PE_CI_RUNS_URL). First scripts/ci_status.py
#     on its own: it judges a run by its jobs (coordinator), so a cancelled run with a failed job, or
#     with a job that started and was stopped, is red; a run with a platform GitHub never gave a runner has no
#     verdict, whatever its own conclusion, and so has a run cancelled before any job started: the check looks past
#     both, judging 5 runs at most before it calls the state unknown (3). It counts no pull request's run (a fork's
#     own branch may be called main), says an answer it can't read is unreadable (3), and never exits 1 (red) on a
#     malformed one (4). For the triage watchdog it names each red
#     run once, with the commits a platform back from runs without a runner left unverified, and reports a platform
#     with no real verdict for 3 runs in a row or for 24 h, once. scripts/tier2_digests.py compares the platforms
#     that ran and notes the one that didn't, and scripts/tier2.sh's annotation of a failed step carries a failed
#     test's panic and what a failed linker said. Then the gate: with pe.gate.tier2 on, a red run fails it until
#     scripts/tier2_explained.txt names the run; with note, it is only reported; a malformed answer, and an answer
#     that never finishes, don't block it (the hook runs this step while git holds main's locks, so the check gives
#     up after 20 s in all); and an unknown mode is an error. The check's cache (--cache) answers for a run it has
#     judged without reading its jobs again, judges a re-run afresh, and lives in the clone's git folder. A token
#     file (--token-file; the gate's pe.gate.ghtoken) is read with a byte-order mark and blanks around the token, and
#     a malformed one is noted; the token goes only to GitHub's API over https, never across a redirect, is never
#     printed, and is dropped when GitHub refuses it (the call is made again without it).
export PE_CI_REPO=example/tier2-fixture
fx="$tmp/fixture"; mkdir -p "$fx"; fxw=$fx; [ "$win" = 1 ] && fxw=$(cygpath -m "$fx")
fxu="file:///${fxw#/}"
jobs_json() {  # run id, then one "name|conclusion|started" per job (started: 1 if GitHub gave it a runner)
  local id=$1 t n c out="" sep=""; shift
  for t in "$@"; do
    n=${t%%|*}; t=${t#*|}; c=${t%%|*}
    if [ "${t#*|}" = 1 ]; then out="$out$sep{\"name\":\"$n\",\"conclusion\":\"$c\",\"runner_name\":\"GitHub Actions 1\",\"steps\":[{\"name\":\"Run\",\"conclusion\":\"$c\"}]}"
    else out="$out$sep{\"name\":\"$n\",\"conclusion\":\"$c\",\"runner_name\":\"\",\"steps\":[]}"; fi
    sep=","
  done
  printf '{"total_count":%s,"jobs":[%s]}' "$#" "$out" > "$fx/jobs-$id.json"
}
run_json() {  # id, conclusion, [event, [head repository, [created at, [attempt]]]]: one run as GitHub lists it
  printf '{"id":%s,"run_attempt":%s,"conclusion":"%s","head_sha":"%sabcdef0123456789","html_url":"https://example.invalid/%s","created_at":"%s","event":"%s","head_branch":"main","head_repository":{"full_name":"%s"},"jobs_url":"%s/jobs-%s.json"}' \
    "$1" "${6:-1}" "$2" "$1" "$1" "${5:-2026-11-17T00:00:00Z}" "${3:-push}" "${4:-$PE_CI_REPO}" "$fxu" "$1"
}
jobs_json 424242 "tier 2, macos-arm64|failure|1" "native audit and secret scan|success|1"
jobs_json 424243; jobs_json 424245  # cancelled while waiting: no job started
jobs_json 424244 "tier 2, macos-arm64|success|1" "native audit and secret scan|success|1"
jobs_json 424250 "tier 2, linux-x64|failure|1" "tier 2, macos-arm64|cancelled|0"
jobs_json 424251 "tier 2, windows-x64|cancelled|1" "tier 2, linux-x64|success|1"
jobs_json 424252 "tier 2, macos-arm64|cancelled|0" "tier 2, linux-x64|success|1"
jobs_json 424253 "tier 2, macos-arm64|cancelled|0" "tier 2, linux-x64|success|1" "tier 2, the same digests on all four|success|1"
printf '{"workflow_runs":[%s,%s]}\n' "$(run_json 424243 cancelled)" "$(run_json 424242 failure)" > "$fx/red.json"
printf '{"workflow_runs":[%s,%s]}\n' "$(run_json 424245 cancelled)" "$(run_json 424244 success schedule)" > "$fx/green.json"
# A fork's pull request from its own branch called main: green above main's red push, then red above main's green.
printf '{"workflow_runs":[%s,%s]}\n' "$(run_json 424247 success pull_request someone/tier2-fixture)" \
  "$(run_json 424242 failure)" > "$fx/fork-green.json"
printf '{"workflow_runs":[%s,%s]}\n' "$(run_json 424248 failure pull_request someone/tier2-fixture)" \
  "$(run_json 424244 success)" > "$fx/fork-red.json"
printf '{"workflow_runs":"not a list"}\n' > "$fx/malformed.json"
git -C "$R" show "$sha:scripts/ci_status.py" > "$tmp/ci_status.py" || die "$rev has no scripts/ci_status.py"
: > "$tmp/explained-none.txt"
ci() {  # name, listing file, wanted exit, regex its last line must match, [cache file]: ci_status.py --check on its own
  local name=$1 f=$2 want=$3 re=$4 rc
  PE_CI_RUNS_URL="$fxu/$f" "$py" -I "$tmp/ci_status.py" --check "$tmp/explained-none.txt" ${5:+--cache "$5"} \
    > "$tmp/$name.log" 2>&1
  rc=$?
  if [ "$rc" = "$want" ] && tail -1 "$tmp/$name.log" | grep -Eq -- "$re"; then
    pass=$((pass + 1)); echo "test_gate: ok   $name (ci_status exit $rc)"
  else
    bad=$((bad + 1)); echo "test_gate: BAD  $name (ci_status exit $rc, wanted $want and /$re/)"; sed 's/^/    /' "$tmp/$name.log"
  fi
}
ci ci-looks-past-cancelled-red red.json 1 "^ci_status: tier 2 RED: run 424242 \(failure\) .*jobs tier 2, macos-arm64: .*past 1 run"
ci ci-looks-past-cancelled-green green.json 0 "^ci_status: tier 2 green: run 424244 .*past 1 run"
ci ci-fork-green-hides-no-red fork-green.json 1 "^ci_status: tier 2 RED: run 424242 \(failure\)"
ci ci-fork-red-blocks-nothing fork-red.json 0 "^ci_status: tier 2 green: run 424244 on"
ci ci-unreadable missing.json 3 "^ci_status: GitHub can't be read"
ci ci-malformed malformed.json 4 "^ci_status: failed: "
# Judged by its jobs: a cancelled run with a failed job is red, and the job never given a runner isn't named; a job
# that started and was cancelled (stopped at its time limit, or by hand) is red; a platform never given a runner
# leaves the run with no verdict, even when GitHub calls the run a failure.
printf '{"workflow_runs":[%s,%s]}\n' "$(run_json 424250 cancelled)" "$(run_json 424244 success)" > "$fx/cancelled-red.json"
printf '{"workflow_runs":[%s,%s]}\n' "$(run_json 424251 cancelled)" "$(run_json 424244 success)" > "$fx/stopped.json"
printf '{"workflow_runs":[%s,%s]}\n' "$(run_json 424252 success)" "$(run_json 424242 failure)" > "$fx/not-run.json"
printf '{"workflow_runs":[%s,%s]}\n' "$(run_json 424253 failure)" "$(run_json 424244 success)" > "$fx/not-run-failure.json"
ci ci-cancelled-run-with-a-failed-job-is-red cancelled-red.json 1 \
  "^ci_status: tier 2 RED: run 424250 \(cancelled\) on 424250abcd, jobs tier 2, linux-x64: https://example\.invalid/424250$"
ci ci-started-then-cancelled-is-red stopped.json 1 "^ci_status: tier 2 RED: run 424251 \(cancelled\) .*jobs tier 2, windows-x64: "
ci ci-not-run-gives-no-verdict not-run.json 1 \
  "^ci_status: tier 2 RED: run 424242 \(failure\) .*\(past 1 run\(s\) with no verdict; not run: macos-arm64 in run 424252\)$"
ci ci-not-run-even-in-a-failure not-run-failure.json 0 \
  "^ci_status: tier 2 green: run 424244 on 424244abcd \(past 1 run\(s\) with no verdict; not run: macos-arm64 in run 424253\)$"
# Each judged run costs an API call (60 an hour), so --check judges 5 at most: six runs that macos-arm64 missed,
# before a red one, leave tier 2's state unknown (3), and the gate passes with a NOTE (chunks-reviewer's L1).
gap=""; for i in 424285 424284 424283 424282 424281 424280; do
  jobs_json $i "tier 2, macos-arm64|cancelled|0" "tier 2, linux-x64|success|1"; gap="$gap$(run_json $i success),"
done
printf '{"workflow_runs":[%s%s]}\n' "$gap" "$(run_json 424242 failure)" > "$fx/long-gap.json"
ci ci-judges-5-runs-at-most long-gap.json 3 \
  "^ci_status: no verdict in the 5 newest runs judged \(not run: macos-arm64 in run 424285; .*; macos-arm64 in run 424281\); older runs not read$"
# --cache (coordinator) keeps a finished run's verdict under its id and attempt, so a later check reads
# only the listing. red-gone.json lists red.json's runs with jobs that can't be read: unreadable (3) without a cache,
# and the same red run as before from a cache that holds them. A re-run (a new attempt) is judged afresh; a cache
# that isn't JSON, or whose entries aren't in its shape, is judged past (and rewritten); one that can't be written
# changes no answer and leaves no temporary file behind; a run whose jobs aren't all finished isn't kept, nor a
# failure that lists no jobs.
sed 's#/jobs-#/gone-#g' "$fx/red.json" > "$fx/red-gone.json"
red_re="^ci_status: tier 2 RED: run 424242 \(failure\) .*jobs tier 2, macos-arm64: .*past 1 run"
ci ci-red-gone-is-unreadable-without-a-cache red-gone.json 3 "^ci_status: GitHub can't be read"
ci ci-cache-first-check-reads-the-jobs red.json 1 "$red_re" "$tmp/c1.json"
ci ci-cache-answers-without-the-jobs red-gone.json 1 "$red_re" "$tmp/c1.json"
jobs_json 424290 "tier 2, linux-x64|failure|1"
printf '{"workflow_runs":[%s]}\n' "$(run_json 424290 failure)" > "$fx/attempt1.json"
ci ci-cache-attempt-1-red attempt1.json 1 "^ci_status: tier 2 RED: run 424290 \(failure\)" "$tmp/c2.json"
jobs_json 424290 "tier 2, linux-x64|success|1"  # the re-run passed
printf '{"workflow_runs":[%s]}\n' "$(run_json 424290 success push "$PE_CI_REPO" 2026-11-17T00:00:00Z 2)" > "$fx/attempt2.json"
ci ci-cache-a-re-run-is-judged-afresh attempt2.json 0 "^ci_status: tier 2 green: run 424290 on 424290abcd$" "$tmp/c2.json"
printf 'not JSON' > "$tmp/c3.json"
ci ci-cache-not-json-is-judged-past red.json 1 "$red_re" "$tmp/c3.json"
ci ci-cache-not-json-is-rewritten red-gone.json 1 "$red_re" "$tmp/c3.json"
printf '{"424242:1":"green","424243:1":{"verdict":"green"}}' > "$tmp/c4.json"
ci ci-cache-entry-out-of-shape-is-judged-past red.json 1 "$red_re" "$tmp/c4.json"
mkdir "$tmp/c5.json"
ci ci-cache-unwritable-changes-no-answer red.json 1 "$red_re" "$tmp/c5.json"
if ls "$tmp"/c5.json.*.tmp > /dev/null 2>&1; then
  bad=$((bad + 1)); echo "test_gate: BAD  ci-cache-unwritable-leaves-no-temporary-file"; ls "$tmp"/c5.json.*.tmp | sed 's/^/    /'
else
  pass=$((pass + 1)); echo "test_gate: ok   ci-cache-unwritable-leaves-no-temporary-file"
fi
# A run with a job that has no conclusion yet (GitHub may list a run as finished before its jobs) is judged, but
# not kept: the next check reads its jobs again.
printf '{"total_count":2,"jobs":[{"name":"tier 2, linux-x64","conclusion":"failure","runner_name":"GitHub Actions 1","steps":[{"name":"Run","conclusion":"failure"}]},{"name":"tier 2, macos-arm64","conclusion":null,"runner_name":"","steps":[]}]}' \
  > "$fx/jobs-424291.json"
printf '{"workflow_runs":[%s]}\n' "$(run_json 424291 failure)" > "$fx/unfinished.json"
sed 's#/jobs-#/gone-#g' "$fx/unfinished.json" > "$fx/unfinished-gone.json"
ci ci-cache-unfinished-jobs-judged unfinished.json 1 "^ci_status: tier 2 RED: run 424291 \(failure\) .*jobs tier 2, linux-x64: " \
  "$tmp/c6.json"
ci ci-cache-unfinished-jobs-not-kept unfinished-gone.json 3 "^ci_status: GitHub can't be read" "$tmp/c6.json"
# A finished run that lists no jobs is kept only when it was cancelled before any started (424243, above) or
# couldn't start: a failure listing none is judged by its own conclusion but read again next time, since its jobs
# may show no verdict (chunks-reviewer's nit).
jobs_json 424292; jobs_json 424293
printf '{"workflow_runs":[%s]}\n' "$(run_json 424292 failure)" > "$fx/no-jobs.json"
printf '{"workflow_runs":[%s]}\n' "$(run_json 424293 startup_failure)" > "$fx/no-start.json"
sed 's#/jobs-#/gone-#g' "$fx/no-jobs.json" > "$fx/no-jobs-gone.json"
sed 's#/jobs-#/gone-#g' "$fx/no-start.json" > "$fx/no-start-gone.json"
ci ci-cache-failure-without-jobs-judged no-jobs.json 1 \
  "^ci_status: tier 2 RED: run 424292 \(failure\) on 424292abcd, jobs \(none named\): " "$tmp/c7.json"
ci ci-cache-failure-without-jobs-not-kept no-jobs-gone.json 3 "^ci_status: GitHub can't be read" "$tmp/c7.json"
ci ci-cache-startup-failure-judged no-start.json 1 "^ci_status: tier 2 RED: run 424293 \(startup_failure\) " "$tmp/c8.json"
ci ci-cache-startup-failure-kept no-start-gone.json 1 "^ci_status: tier 2 RED: run 424293 \(startup_failure\) " "$tmp/c8.json"
# The token (--token-file), first in ci_status.py's own functions: it goes to GitHub's API over https and to no other
# address, rides in a header a redirect doesn't carry on, is read from its file with a byte-order mark and blanks
# around it (anything else is no token, noted without its text), and is dropped when GitHub refuses it (HTTP 401 or
# 403): the call is made again without it, as is the rest of the run's. A local server stands in for the API there.
# The fixture token is built here, so no token-shaped text sits in this file for the secret scan to find.
tok="fixture_$(printf 'a%.0s' $(seq 32))_token"
tmpw=$tmp; [ "$win" = 1 ] && tmpw=$(cygpath -m "$tmp")
"$py" -I - "$tmpw" "$tok" > "$tmp/token-unit.log" 2>&1 << 'PYEOF'
import http.server, os, sys, threading, urllib.request
tmp, tok = sys.argv[1], sys.argv[2]
sys.path.insert(0, tmp)
import ci_status as m


def check(name, ok, detail=""):
    print(f"ok {name}" if ok else f"BAD {name} {detail}")


m.auth["token"] = tok
api = "https://api.github.com/repos/o/r/actions/runs"
others = ["http://api.github.com/x", "https://api.github.com:8443/x", "https://api.github.com:bad/x",
          "https://api.github.com.example.invalid/x", "https://api.github.com@example.invalid/x",
          "https://user@api.github.com/x", "https://example.invalid/x", "file:///c:/x.json", "http://127.0.0.1:1/x"]
r = m.request(api)
check("ci-token-goes-only-to-githubs-api",
      m.token_for(api) == tok and r.unredirected_hdrs.get("Authorization") == "Bearer " + tok
      and "Authorization" not in r.headers and all(m.token_for(u) is None for u in others)
      and not any(m.request(u).has_header("Authorization") for u in others),
      "(it went elsewhere, or not to the API)")
new = urllib.request.HTTPRedirectHandler().redirect_request(r, None, 302, "Found", {}, "https://example.invalid/y")
check("ci-token-not-carried-on-a-redirect", new is not None and not new.has_header("Authorization"))
m.auth["token"] = None
check("ci-no-token-without-a-file", m.token_for(api) is None and not m.request(api).has_header("Authorization"))


def tf(name, data):
    p = os.path.join(tmp, name)
    with open(p, "wb") as f:
        f.write(data)
    return p


t = tok.encode()
good = [tf("tok1", t + b"\n"), tf("tok2", b"  " + t + b"\r\n"), tf("tok3", b"\xef\xbb\xbf" + t),
        tf("tok4", b"\xff\xfe" + (tok + "\r\n").encode("utf-16-le"))]
none = [os.path.join(tmp, "no-such-token-file"), tf("tok5", b""), tf("tok6", b" \r\n")]
bad = [tf("tok7", b"Bearer " + t), tf("tok8", t + b"\n" + t), tf("tok9", b"a" * 5000),
       tf("tok10", b"\xff\xfe\x00\xd8")]
m.auth["notes"] = []
got_good, got_none = [m.load_token(p) for p in good], [m.load_token(p) for p in none]
quiet = not m.auth["notes"]
got_bad = [m.load_token(p) for p in bad]
check("ci-token-file-rules", got_good == [tok] * 4 and got_none == [None] * 3 and quiet and got_bad == [None] * 4
      and m.auth["notes"] == ["the token file holds no token, so GitHub was read without one"] * 4,
      f"(read {[g == tok for g in got_good]}, none {got_none}, quiet {quiet},"
      f" malformed {[g is None for g in got_bad]}, {len(m.auth['notes'])} notes)")
seen = []


class Api(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        a = self.headers.get("Authorization")
        seen.append(a)
        code = (401 if self.path.startswith("/401") else 403) if a else 200
        body = b'{"ok": 1}' if code == 200 else b'{"message": "Bad credentials"}'
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


srv = http.server.HTTPServer(("127.0.0.1", 0), Api)
threading.Thread(target=srv.serve_forever, daemon=True).start()
base = f"http://127.0.0.1:{srv.server_address[1]}"
m.token_for = lambda url: m.auth["token"]  # this server stands in for GitHub's API
results = []
for code in ("401", "403"):
    m.auth["token"], m.auth["notes"] = tok, []
    del seen[:]
    try:
        got = [m.get(f"{base}/{code}/x"), m.get(f"{base}/{code}/y")]
    except Exception as e:  # a refusal that isn't handled: Unreadable, or endless retries
        got = [type(e).__name__]
    results.append((code, got == [{"ok": 1}] * 2, seen == ["Bearer " + tok, None, None], m.auth["token"] is None,
                    m.auth["notes"] == [f"GitHub refused the token (HTTP {code}), so GitHub was read without it"]))
srv.shutdown()
check("ci-token-refused-is-dropped", all(all(x[1:]) for x in results),
      f"(answers, headers, dropped, noted: {results})")
print("done")
PYEOF
while read -r w n rest; do
  case $w in
    ok) pass=$((pass + 1)); echo "test_gate: ok   $n" ;;
    BAD) bad=$((bad + 1)); echo "test_gate: BAD  $n $rest" ;;
  esac
done < "$tmp/token-unit.log"
if ! grep -qx "done" "$tmp/token-unit.log" || [ "$(grep -c -E '^(ok|BAD) ' "$tmp/token-unit.log")" != 5 ]; then
  bad=$((bad + 1)); echo "test_gate: BAD  ci-token-checks-all-ran"; sed 's/^/    /' "$tmp/token-unit.log"
fi
# ...then end to end: with a token in its file, the check reads as before and prints no token; a file holding
# anything else is noted on the check's last line (the line the gate shows) and in the watchdog's output.
printf '%s\n' "$tok" > "$tmp/token.txt"
printf 'Bearer %s\n' "$tok" > "$tmp/token-bad.txt"
nb="the token file holds no token, so GitHub was read without one"
PE_CI_RUNS_URL="$fxu/red.json" "$py" -I "$tmp/ci_status.py" --token-file "$tmp/token.txt" \
  --check "$tmp/explained-none.txt" > "$tmp/token-e2e.log" 2>&1; r1=$?
PE_CI_RUNS_URL="$fxu/red.json" "$py" -I "$tmp/ci_status.py" --token-file "$tmp/token-bad.txt" \
  --check "$tmp/explained-none.txt" > "$tmp/token-e2e-bad.log" 2>&1; r2=$?
PE_CI_RUNS_URL="$fxu/red.json" "$py" -I "$tmp/ci_status.py" --token-file "$tmp/token-bad.txt" \
  --new "$tmp/t2state-token" > "$tmp/token-e2e-new.log" 2>&1; r3=$?
if [ "$r1$r2$r3" = 110 ] \
  && tail -1 "$tmp/token-e2e.log" | grep -Eq "^ci_status: tier 2 RED: run 424242 \(failure\) .*with no verdict\)$" \
  && tail -1 "$tmp/token-e2e-bad.log" | grep -Eq "^ci_status: tier 2 RED: run 424242 \(failure\) .*with no verdict\); $nb$" \
  && grep -qx "ci_status: note: $nb" "$tmp/token-e2e-new.log" \
  && ! grep -qF -- "$tok" "$tmp/token-e2e.log" "$tmp/token-e2e-bad.log" "$tmp/token-e2e-new.log"; then
  pass=$((pass + 1)); echo "test_gate: ok   ci-token-file-read-and-never-printed"
else
  bad=$((bad + 1)); echo "test_gate: BAD  ci-token-file-read-and-never-printed (exits $r1 $r2 $r3)"
  sed 's/^/    /' "$tmp/token-e2e.log" "$tmp/token-e2e-bad.log" "$tmp/token-e2e-new.log"
fi
PE_CI_RUNS_URL="$fxu/red.json" "$py" -I "$tmp/ci_status.py" --new "$tmp/t2state" > "$tmp/new1.log" 2>&1; r1=$?
PE_CI_RUNS_URL="$fxu/red.json" "$py" -I "$tmp/ci_status.py" --new "$tmp/t2state" > "$tmp/new2.log" 2>&1; r2=$?
if [ "$r1$r2" = 00 ] && [ "$(cat "$tmp/new1.log")" = "TIER2 RED 424242 failure https://example.invalid/424242 jobs: tier 2, macos-arm64" ] \
  && [ ! -s "$tmp/new2.log" ]; then
  pass=$((pass + 1)); echo "test_gate: ok   ci-new-names-a-red-run-once"
else
  bad=$((bad + 1)); echo "test_gate: BAD  ci-new-names-a-red-run-once (exits $r1 $r2)"; sed 's/^/    /' "$tmp/new1.log" "$tmp/new2.log"
fi
# The watchdog's record of each platform: macos-arm64 never given a runner in 3 runs in a row, and never verified,
# is reported once. Verified in 424270, then without a runner in 424271: reported 24 h after 424270, not at 23.5 h;
# and when it is back in the red run 424272, that run's line names the commits it left unverified.
for i in 424260 424261 424262 424271; do jobs_json $i "tier 2, macos-arm64|cancelled|0" "tier 2, linux-x64|success|1"; done
jobs_json 424270 "tier 2, macos-arm64|success|1" "tier 2, linux-x64|success|1"
jobs_json 424272 "tier 2, macos-arm64|failure|1" "tier 2, linux-x64|success|1"
printf '{"workflow_runs":[%s,%s,%s]}\n' "$(run_json 424262 success)" "$(run_json 424261 success)" \
  "$(run_json 424260 success)" > "$fx/misses3.json"
r70=$(run_json 424270 success push "$PE_CI_REPO" 2026-11-17T00:00:00Z)
r71=$(run_json 424271 success push "$PE_CI_REPO" 2026-11-17T20:00:00Z)
r72=$(run_json 424272 failure push "$PE_CI_REPO" 2026-11-18T01:00:00Z)
printf '{"workflow_runs":[%s]}\n' "$r70" > "$fx/day1.json"
printf '{"workflow_runs":[%s,%s]}\n' "$r71" "$r70" > "$fx/day2.json"
printf '{"workflow_runs":[%s,%s,%s]}\n' "$r72" "$r71" "$r70" > "$fx/day3.json"
nw() {  # listing, state file, the time: one --new call; appends "<exit>: <output>" to $tmp/<state file>.out
  local o
  o=$(PE_CI_RUNS_URL="$fxu/$1" PE_CI_NOW=$3 "$py" -I "$tmp/ci_status.py" --new "$tmp/$2" 2>&1)
  printf '%s: %s\n' "$?" "$o" >> "$tmp/$2.out"
}
nwant() {  # check name, state file, wanted lines of $tmp/<state file>.out
  local name=$1 f=$2; shift 2
  if [ "$(cat "$tmp/$f.out")" = "$(printf '%s\n' "$@")" ]; then
    pass=$((pass + 1)); echo "test_gate: ok   $name"
  else
    bad=$((bad + 1)); echo "test_gate: BAD  $name"; printf '    want: %s\n' "$@"; sed 's/^/    got:  /' "$tmp/$f.out"
  fi
}
nw misses3.json s3 2026-11-17T03:00:00Z; nw misses3.json s3 2026-11-17T04:00:00Z
nwant ci-new-stale-after-3-runs-once s3 \
  "0: TIER2 STALE macos-arm64 unverified since tier 2 began (no real verdict yet): 3 run(s) in a row without a runner, 3 h without a real verdict" \
  "0: "
nw day1.json s24 2026-11-17T00:10:00Z; nw day2.json s24 2026-11-17T23:30:00Z; nw day2.json s24 2026-11-18T00:30:00Z
nw day3.json s24 2026-11-18T01:30:00Z
nwant ci-new-stale-after-24-h-and-the-range-when-back s24 "0: " "0: " \
  "0: TIER2 STALE macos-arm64 unverified since 424270abcd: 1 run(s) in a row without a runner, 24 h without a real verdict" \
  "0: TIER2 RED 424272 failure https://example.invalid/424272 jobs: tier 2, macos-arm64; macos-arm64 unverified since 424270abcd, up to 424272abcd"
# scripts/tier2_digests.py: a platform with no folder (no output kept) is noted and not compared, and if it is the
# reference, the first that ran stands in; a folder without digests, or a line that differs, fails.
git -C "$R" show "$sha:scripts/tier2_digests.py" > "$tmp/tier2_digests.py" || die "$rev has no scripts/tier2_digests.py"
dg="$tmp/digests"
for p in a/tier2-windows-x64 a/tier2-linux-x64 a/tier2-linux-arm64 c/tier2-linux-x64; do
  mkdir -p "$dg/$p/digests"; printf 'RESULT a 1\nRESULT b 2\n' > "$dg/$p/digests/debug.txt"
  printf 'RESULT c 3\n' > "$dg/$p/digests/release.txt"
done
printf 'RESULT c 4\n' > "$dg/c/tier2-linux-x64/digests/release.txt"
mkdir -p "$dg/b/tier2-macos-arm64"
dgc() {  # name, wanted exit, a regex some line must match, a regex the last line must match, then the folders
  local name=$1 want=$2 re=$3 last=$4 rc; shift 4
  "$py" -I "$tmp/tier2_digests.py" "$@" > "$tmp/$name.log" 2>&1; rc=$?
  if [ "$rc" = "$want" ] && grep -Eq -- "$re" "$tmp/$name.log" && tail -1 "$tmp/$name.log" | grep -Eq -- "$last"; then
    pass=$((pass + 1)); echo "test_gate: ok   $name (tier2_digests exit $rc)"
  else
    bad=$((bad + 1)); echo "test_gate: BAD  $name (tier2_digests exit $rc, wanted $want, /$re/, /$last/)"
    sed 's/^/    /' "$tmp/$name.log"
  fi
}
dgc digests-not-run-is-noted 0 "^tier2_digests: macos-arm64: not run \(no output kept\); not compared$" \
  "^tier2_digests: PASS: windows-x64, linux-x64, linux-arm64 have the same digests$" \
  "$dg/a/tier2-windows-x64" "$dg/a/tier2-linux-x64" "$dg/a/tier2-linux-arm64" "$dg/none/tier2-macos-arm64"
dgc digests-reference-not-run 0 "^tier2_digests: windows-x64 didn't run; linux-x64 is the reference$" \
  "^tier2_digests: PASS: linux-x64, linux-arm64 have the same digests$" \
  "$dg/none/tier2-windows-x64" "$dg/a/tier2-linux-x64" "$dg/a/tier2-linux-arm64"
dgc digests-folder-without-digests-fails 1 "^tier2_digests: DIFFERENT: release, macos-arm64: no digests" \
  "^tier2_digests: FAIL: 2 difference" "$dg/a/tier2-windows-x64" "$dg/b/tier2-macos-arm64"
dgc digests-a-differing-line-fails 1 "^  linux-x64: RESULT c 4$" "^tier2_digests: FAIL: 1 difference" \
  "$dg/a/tier2-windows-x64" "$dg/c/tier2-linux-x64"
# scripts/tier2.sh's annotation of a failed step, which is all of a hosted failure the public API shows: a failed
# test's panic, never a should_panic test's own nor a failed test's other output; and what a failed linker said,
# each line once, without its command line or the note that arguments were omitted.
git -C "$R" show "$sha:scripts/tier2.sh" > "$tmp/tier2.sh" || die "$rev has no scripts/tier2.sh"
cat > "$tmp/t2-tests.log" << 'EOF'
running 2 tests
test tests::a_panics_on_purpose - should panic ... ok
test tests::b_fails ... FAILED

successes:

---- tests::a_panics_on_purpose stdout ----

thread 'tests::a_panics_on_purpose' (1) panicked at src/lib.rs:10:9:
on purpose

successes:
    tests::a_panics_on_purpose

failures:

---- tests::b_fails stdout ----
some output first

thread 'tests::b_fails' (2) panicked at src/lib.rs:15:9:
assertion `left == right` failed
  left: 1
 right: 2

failures:
    tests::b_fails

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: test failed, to rerun pass `--lib`
EOF
cat > "$tmp/t2-link.log" << 'EOF'
error: linking with `rust-lld` failed: signal: 6 (SIGABRT)
  |
  = note: "rust-lld" "-flavor" "wasm" "-o" "script.wasm"
  = note: some arguments are omitted. use `--verbose` to show all linker arguments
  = note: rust-lld: error: out of memory
          libc++abi: terminating

error: could not compile `script` (lib) due to 1 previous error
error: linking with `rust-lld` failed: signal: 6 (SIGABRT)
  |
  = note: "rust-lld" "-flavor" "wasm" "-o" "script.wasm"
  = note: rust-lld: error: out of memory
          libc++abi: terminating

error: could not compile `script` (lib) due to 1 previous error
EOF
ann() {  # log: the annotation tier2.sh writes for a step that failed with it, one decoded line per line
  ( cr=$(printf '\r'); where=fixture; eval "$(sed -n '/^annotate() {/,/^}/p' "$tmp/tier2.sh")"
    GITHUB_ACTIONS=true annotate step 101 "$1" ) | sed 's/%0A/\n/g; s/%25/%/g'
}
ann "$tmp/t2-tests.log" > "$tmp/ann-tests.txt"; ann "$tmp/t2-link.log" > "$tmp/ann-link.txt"
n_of() { grep -c -F -x -- "$2" "$tmp/$1"; }  # file, line: how many lines are exactly that
if [ "$(n_of ann-tests.txt 'assertion `left == right` failed')" = 1 ] && [ "$(n_of ann-tests.txt ' right: 2')" = 1 ] \
  && [ "$(n_of ann-tests.txt 'test tests::b_fails ... FAILED')" = 1 ] && ! grep -qE "on purpose|some output first" "$tmp/ann-tests.txt"; then
  pass=$((pass + 1)); echo "test_gate: ok   tier2-annotation-has-the-failed-tests-panic"
else
  bad=$((bad + 1)); echo "test_gate: BAD  tier2-annotation-has-the-failed-tests-panic"; sed 's/^/    /' "$tmp/ann-tests.txt"
fi
if [ "$(n_of ann-link.txt 'linker: rust-lld: error: out of memory')" = 1 ] \
  && [ "$(n_of ann-link.txt 'linker: libc++abi: terminating')" = 1 ] \
  && [ "$(n_of ann-link.txt 'error: linking with `rust-lld` failed: signal: 6 (SIGABRT)')" = 1 ] \
  && ! grep -qE -- "-flavor|arguments are omitted" "$tmp/ann-link.txt"; then
  pass=$((pass + 1)); echo "test_gate: ok   tier2-annotation-has-what-the-linker-said"
else
  bad=$((bad + 1)); echo "test_gate: BAD  tier2-annotation-has-what-the-linker-said"; sed 's/^/    /' "$tmp/ann-link.txt"
fi
# A test that prints pe-logic's BuildError with {:?} has the linker's notes on one line, escaped. And the linker's
# words come first, inside the 4096 characters GitHub keeps, however many tests failed (tier 2's macOS, 2026-11-18).
cat > "$tmp/t2-link-debug.log" << 'EOF'
running 1 test
test a_voter_builds_checks_loads_and_attests_once ... FAILED

failures:

---- a_voter_builds_checks_loads_and_attests_once stdout ----

thread '<unnamed>' (7) panicked at crates/pe-app/tests/builds.rs:129:46:
the lamp builds: Build(Rustc { text: "error: linking with `rust-lld` failed: signal: 6 (SIGABRT)\n  = note: \"rust-lld\" \"-flavor\" \"wasm\"\n  = note: rust-lld: error: out of memory\n          libc++abi: terminating\n     Locking 1 package to highest Rust 1.99.0 compatible version\nerror: could not compile `script` (lib) due to 1 previous error\n" })

failures:
    a_voter_builds_checks_loads_and_attests_once

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
EOF
for i in $(seq 10 59); do
  echo "test a_test_whose_name_is_as_long_as_those_of_tier_2s_macos_runs_that_fail_on_a_crashed_linker_$i ... FAILED"
done > "$tmp/t2-many.log"
cat "$tmp/t2-link.log" >> "$tmp/t2-many.log"
printf 'error: useless conversion to the same type: `i64`\n  --> crates/pe-app/src/clock.rs:65:18\n' \
  >> "$tmp/t2-many.log"
for i in $(seq 10 59); do printf 'warning: unused variable: `x%s`\n  --> src/lib.rs:%s:9\n' "$i" "$i"; done \
  >> "$tmp/t2-many.log"
echo 'error: test failed, to rerun pass `-p pe-render --test gpu`' >> "$tmp/t2-many.log"
ann "$tmp/t2-link-debug.log" > "$tmp/ann-debug.txt"; ann "$tmp/t2-many.log" > "$tmp/ann-many.txt"
if [ "$(n_of ann-debug.txt 'linker: rust-lld: error: out of memory')" = 1 ] \
  && [ "$(n_of ann-debug.txt 'linker: libc++abi: terminating')" = 1 ] \
  && ! grep -qE "^linker: .*(flavor|Locking|could not compile)" "$tmp/ann-debug.txt"; then
  pass=$((pass + 1)); echo "test_gate: ok   tier2-annotation-unfolds-a-debug-printed-build-error"
else
  bad=$((bad + 1)); echo "test_gate: BAD  tier2-annotation-unfolds-a-debug-printed-build-error"
  sed 's/^/    /' "$tmp/ann-debug.txt"
fi
if [ "$(head -c 4000 "$tmp/ann-many.txt" | grep -c -F -x 'linker: rust-lld: error: out of memory')" = 1 ]; then
  pass=$((pass + 1)); echo "test_gate: ok   tier2-annotation-has-the-linker-in-what-github-keeps"
else
  bad=$((bad + 1)); echo "test_gate: BAD  tier2-annotation-has-the-linker-in-what-github-keeps"
  head -c 600 "$tmp/ann-many.txt" | sed 's/^/    /'; echo
fi
# Every failed binary's error line is inside them too, however many tests failed and however many warnings, each
# with its place, the build printed before it (chunks-reviewer's L1).
last_bin='error: test failed, to rerun pass `-p pe-render --test gpu`'
if [ "$(head -c 4000 "$tmp/ann-many.txt" | grep -c -F -x -- "$last_bin")" = 1 ] \
  && [ "$(head -c 4000 "$tmp/ann-many.txt" | grep -c -F -x -- '  --> crates/pe-app/src/clock.rs:65:18')" = 1 ] \
  && ! grep -q -F -- '--> src/lib.rs:' "$tmp/ann-many.txt"; then
  pass=$((pass + 1)); echo "test_gate: ok   tier2-annotation-names-every-failed-binary"
else
  bad=$((bad + 1)); echo "test_gate: BAD  tier2-annotation-names-every-failed-binary"
  head -c 600 "$tmp/ann-many.txt" | sed 's/^/    /'; echo
fi
# A panic can be one line of several kB: cut to 300 characters, it leaves room for the next test's panic.
{ printf 'running 2 tests\ntest a_long_panic ... FAILED\ntest b_short_panic ... FAILED\n\nfailures:\n\n'
  printf -- "---- a_long_panic stdout ----\n\nthread 'a_long_panic' (1) panicked at src/lib.rs:1:1:\n"
  head -c 5000 /dev/zero | tr '\0' x; printf '\n\n'
  printf -- "---- b_short_panic stdout ----\n\nthread 'b_short_panic' (2) panicked at src/lib.rs:2:1:\n"
  printf "the second test's own panic message\n\nfailures:\n    a_long_panic\n    b_short_panic\n\n"
  printf 'test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n'
} > "$tmp/t2-long-panic.log"
ann "$tmp/t2-long-panic.log" > "$tmp/ann-long.txt"
if [ "$(head -c 4000 "$tmp/ann-long.txt" | grep -c -F -x "the second test's own panic message")" = 1 ]; then
  pass=$((pass + 1)); echo "test_gate: ok   tier2-annotation-cuts-a-long-panic-line"
else
  bad=$((bad + 1)); echo "test_gate: BAD  tier2-annotation-cuts-a-long-panic-line"; cut -c1-200 "$tmp/ann-long.txt"
fi
export PE_GATE_TIER2=on PE_CI_RUNS_URL="$fxu/red.json"
new_case; echo "tier 2 test" >> "$R/README.md"; commit -m "a change while tier 2 is red"
expect tier2-red fail "^gate: tier 2 RED: run 424242 \(failure\)" "add '<run id> -- <reason>' to scripts/tier2_explained\.txt" "^gate: FAIL"
new_case; echo "424242 -- tier 2 on macos-arm64: a test fixture" >> "$R/scripts/tier2_explained.txt"; commit -m "explain run 424242"
expect tier2-explained pass "^gate: tier 2: tier 2 failure in run 424242, explained in tier2_explained\.txt" \
  "NOTE: this landing changes the gate's allow files" "^gate:   scripts/tier2_explained\.txt$" "^gate: PASS"
new_case; echo "tier 2 test" >> "$R/README.md"; commit -m "a change while tier 2 is red, reported only"
PE_GATE_TIER2=note expect tier2-note pass "^gate: NOTE: tier 2 RED: run 424242" "not blocking yet: pe\.gate\.tier2 is note" "^gate: PASS"
PE_CI_RUNS_URL="$fxu/malformed.json" expect tier2-malformed pass "^gate: NOTE: tier 2's check failed \(exit 4" "^gate: PASS"
PE_GATE_TIER2=maybe expect tier2-unknown-mode error "pe\.gate\.tier2 \(or PE_GATE_TIER2\) is 'maybe'; it must be on, note or off"
# The gate reads the token file the clone names (git config pe.gate.ghtoken): a malformed one is noted on its tier-2
# line, which shows the file was read, and no token text reaches its output.
new_case; echo "tier 2 test" >> "$R/README.md"; commit -m "a change while tier 2 is red, read with a token file"
git -C "$R" config pe.gate.ghtoken "$tmp/token-bad.txt"
expect tier2-token-file-from-git-config fail "^gate: tier 2 RED: run 424242 \(failure\) .*; $nb$" "^gate: FAIL"
git -C "$R" config --unset pe.gate.ghtoken
if grep -qF -- "$tok" "$tmp/tier2-token-file-from-git-config.log"; then
  bad=$((bad + 1)); echo "test_gate: BAD  tier2-token-never-in-the-gates-output"
else
  pass=$((pass + 1)); echo "test_gate: ok   tier2-token-never-in-the-gates-output"
fi
# The gate keeps the check's cache in the clone's git folder: the gates above filled it, so red-gone.json's red run
# still fails the gate while its jobs can't be read. Without that file the same check can't read them, and passes.
PE_CI_RUNS_URL="$fxu/red-gone.json" expect tier2-cache-in-the-clones-git-folder fail \
  "^gate: tier 2 RED: run 424242 \(failure\)" "^gate: FAIL"
rm -f "$(git -C "$R" rev-parse --path-format=absolute --git-common-dir)/pe-tier2-verdicts.json"
PE_CI_RUNS_URL="$fxu/red-gone.json" expect tier2-without-that-file-unreadable pass \
  "^gate: NOTE: tier 2's state is unknown \(GitHub can't be read" "^gate: PASS"
# A server that answers with a status line and then one header byte a second: a socket's own time-out never fires,
# so only the check's 20 s limit can end the call. It serves until the check is over (15 minutes at most; the exit
# trap stops it too), since the gate reaches step 5 only after its scans and audit: on a loaded machine that took
# over the 2 minutes it used to live, and the check found nothing listening (2026-11-17).
cat > "$tmp/stall.py" << 'PYEOF'
import socket, sys, time
end = time.time() + float(sys.argv[2])
s = socket.socket()
s.bind(("127.0.0.1", 0))
s.listen(4)
with open(sys.argv[1], "w") as f:
    f.write(str(s.getsockname()[1]))
s.settimeout(1.0)
held = []
while time.time() < end:
    try:
        c, _ = s.accept()
        c.sendall(b"HTTP/1.1 200 OK\r\nX-Slow: ")
        held.append(c)
    except socket.timeout:
        pass
    for c in list(held):
        try:
            c.sendall(b"a")
        except OSError:
            held.remove(c)
PYEOF
"$py" -I "$tmp/stall.py" "$tmp/stall.port" 900 > "$tmp/stall.log" 2>&1 & spid=$!
for _ in $(seq 100); do [ -s "$tmp/stall.port" ] && break; sleep 0.1; done
[ -s "$tmp/stall.port" ] || die "the stalling server didn't start: $(cat "$tmp/stall.log")"
PE_CI_RUNS_URL="http://127.0.0.1:$(cat "$tmp/stall.port")/runs" expect tier2-stalled-answer-cut-at-20-s pass \
  "^gate: NOTE: tier 2's state is unknown \(GitHub can't be read: no answer within 20 s; (19|2[0-9]) s\); not blocking" "^gate: PASS"
kill "$spid" 2> /dev/null; wait "$spid" 2> /dev/null; spid=
unset PE_GATE_TIER2 PE_CI_RUNS_URL PE_CI_REPO

# 26. The hook, through its trampoline: refuses a merge into main that fails, leaves main where it was, refuses
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
