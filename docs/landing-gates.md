# The landing gate

One command, `scripts/gate.sh`, checks a commit before it lands on main. Today it runs three checks: the
native-code audit, the secret scan and, where the term files are configured, the publication check. The re-founding plan (`docs/refound-plan.md`, 4.6) adds the format,
lint, test, model and harness checks, in two tiers. Owner: WS-ci.

## What it checks

`bash scripts/gate.sh [--base <rev>] [--full-history] [--report <folder>] [<rev>]`

It checks `<rev>` (default HEAD) as it would land on `--base` (default main). It exits 0 on a pass, 1 on a
fail and 2 on an error. It reads only committed content: it exports the tree of `<rev>` and takes the audit
script, the allow file and the scan rules from it. When a landing changes those rules, the gate lists the
changed files, and the landing's reviewer approves those lines with the rest.

1. **Native-code audit** (the "100% Rust" ruling, D1): `scripts/audit_native.py --check`.
   * It finds every Cargo workspace in the tree by itself, so new crates and new workspaces are covered as they
     appear. A crate that is in no audited workspace fails.
   * For each workspace and each target in the allow file, it asks `cargo tree` which packages are built. The
     edges come from `cargo tree`, the real feature resolver, not from `cargo metadata`. The old audit took them
     from metadata, and that made 3 of its 14 Windows flags false: pkg-config, and khronos-egl's.
   * It classifies every package, including the workspace's own crates (see the script's docstring for the
     classes).
   * Every flag needs a line in `scripts/native_allow.txt`, with its reason. Lines can require features to be
     on or off. The audit checks those both for the workspace as a whole and for each crate built on its own:
     `cargo build -p` unifies features over that crate's graph only. For example, blake3 is accepted only with
     `pure`.
2. **Secret scan:** gitleaks, the version pinned in `scripts/install_gitleaks.sh`. It runs with its 222 default
   rules, the project's rules in `.gitleaks.toml` and that file's exact-line exceptions, each with its reason.
   It scans three things: the added lines of every commit in `<base>..<rev>` (or of every commit `<rev>` reaches,
   with `--full-history`), those commits' messages, and the whole tree of `<rev>`. A secret that is added and
   then deleted still fails: the history is what gets published. Inline `gitleaks:allow` comments are ignored.
   Findings print with the secret redacted.
3. **Publication check:** `scripts/publish_check.sh`, the term check that the publishing step already runs.
   * It needs this clone's term file and allowlist: `git config pe.gate.terms <file>` and `pe.gate.allow <file>`
     (or `PE_PUBLISH_TERMS` and `PE_PUBLISH_ALLOW`).
   * It checks the tree of `<rev>`, and the added lines and messages of `<base>..<rev>`. Every term blocks there.
     Identities are left to the publishing step, which maps them.
   * The terms are never published, so hosted CI skips this check.
   * A `--full-history` run doesn't fail on the old history's terms: the user chose to keep that history
     (2026-10-31 11:49Z), and the cleaner replaces those terms.

On main at the gate's landing, the audit checks 7 workspaces and 626 package builds, and every flag is accepted.
The secret scan of the whole history finds nothing.

## Running it

* **Before a landing:** `bash "<team folder>/heavy.sh" bash scripts/gate.sh --base main <branch>`. It takes about
  20 s, most of it `cargo metadata` and `cargo tree`; nothing is built.
* **Install gitleaks once per clone:** `bash scripts/install_gitleaks.sh`. It puts the pinned binary, checked
  against its SHA-256, in `.tools/` at the root of the main checkout, which is git-ignored. To move to a newer
  gitleaks, change the version and the pins together and run `--full-history` in the same change.
* **The hook,** `scripts/hooks/reference-transaction`, refuses any update of main whose new commit fails the
  gate: a merge, a commit, a reset, `update-ref`, or a fetch or push into the repository. Other ref updates pass
  through at once.
  * Enable it once per clone: `git config core.hooksPath "<main checkout>/scripts/hooks"`.
  * Optionally, run the audit under the org's build-slot wrapper: `git config pe.gate.wrap 'bash "<team folder>/heavy.sh"'`.
  * When it refuses a merge in the main checkout, main is unchanged. `git merge --abort` puts the checkout back;
    after a fast-forward or a reset, use `git reset --merge HEAD`.
  * `PE_GATE=off` skips it for one command, in an emergency only. Whoever uses it reports it to the coordinator
    in the same turn.
* **Hosted CI,** `.github/workflows/gate.yml`, runs the same gate on every push to main, every pull request, and
  nightly over the whole history (user ruling Q12).
* **The self-test,** `bash scripts/test_gate.sh`, takes about 7 minutes. In a throwaway clone it plants a secret
  in five ways, native code in four ways and a publication term in two (with a made-up term). It checks that
  the gate refuses each one, names it, and passes a clean change. It does the same for the hook. Run it after changing the gate.

## Accepting an exception

* **Native code:** add an `allow` line (or a `class`, `exclude` or `targets` line) to `scripts/native_allow.txt`.
  Give the reason, with evidence: the build script's lines, and the features it depends on.
* **A false secret:** add an `[[allowlists]]` entry to `.gitleaks.toml` that matches the exact line, and its path,
  with the reason.
* **A line that only looks like a term:** add it to the publication allowlist by its path and line hash
  (`scripts/publish_allow.py`). The allowlist names the line, never the term.

## What it can't see

The audit is a static reading of the build graph and of the files in each package. It doesn't catch:
* a build script that downloads a prebuilt library, or runs a compiler on a file it ships under a name that
  isn't a C, C++, assembly or Objective-C extension, with no helper crate;
* a procedural macro that runs a compiler;
* `#[link]` attributes in dependencies (OS bindings such as windows-sys and libc have them, so they aren't
  flagged). In the workspace's own crates they are flagged.

The secret scan matches patterns and entropy, so a credential in a new format can pass. The term check
(`scripts/publish_check.sh`) covers names and addresses before each public push.
