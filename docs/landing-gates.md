# The landing gate

One command, `scripts/gate.sh`, checks a commit before it lands on main. Today it runs three checks: the
native-code audit, the secret scan and, where the term files are configured, the publication check. The
re-founding plan (`docs/refound-plan.md`, 4.6) adds the format, lint, test, model and harness checks, in two
tiers. Owner: WS-ci.

## What it checks

`bash scripts/gate.sh [--base <rev>] [--full-history] [--report <folder>] [<rev>]`

It checks `<rev>` (default HEAD) as it would land on `--base` (default main). It exits 0 on a pass, 1 on a
fail and 2 on an error.

**The gate never judges a change to itself with the changed code** (coordinator ruling, 2026-11-10):
* When there is a base, the gate's code comes from it: `scripts/gate.sh` (a copy run from the landing hands over
  to the base's at once), `scripts/audit_native.py`, `scripts/install_gitleaks.sh`, the hook, the publication
  check's scripts, the rules of `.gitleaks.toml`, and the cargo and rustup configuration files the audit runs with.
* Only the allow files come from `<rev>`: `scripts/native_allow.txt` and the `[[allowlists]]` entries of
  `.gitleaks.toml`.
* When a landing changes any of these files, the gate prints a NOTE naming them, and the landing's reviewer
  approves those lines. A change to the gate's code takes effect from the next landing. So a new allow-file
  syntax lands after the code that reads it.
* With `--full-history`, or when the base has no gate yet, the code comes from `<rev>`.

**It reads only what is committed, as committed.** It exports the tree of `<rev>` byte for byte, with no
attributes applied, and checks the export against `git ls-tree`. So an `export-ignore` path, an eol rule or a
filter in a committed `.gitattributes` can't hide or change a file. The gate refuses:
* a submodule or a symbolic link;
* a path Windows can't hold as committed (a reserved name, a trailing dot, two paths that differ only in case).

Every git command it runs has in-tree attributes off, so a `-diff` attribute can't hide a line from the history
checks either.

1. **Secret scan:** gitleaks, the build pinned in `scripts/install_gitleaks.sh`.
   * The gate checks the SHA-256 of whichever gitleaks binary it finds against the base's pin, so a binary that a
     landing commits (into a `.tools` folder, say) is never run.
   * It runs with the 222 default rules, the project's rules in `.gitleaks.toml` and that file's exact-line
     exceptions, each with its reason.
   * It scans three things: the added lines of every commit in `<base>..<rev>` (or of every commit `<rev>`
     reaches, with `--full-history`), those commits' messages, and the whole export.
   * A merge is scanned as its diff to its first parent. `git log -p` shows no diff for a merge by default, so a
     secret that only a merge adds would otherwise pass.
   * A secret that is added and then deleted still fails: the history is what gets published.
   * No ignore file counts. Inline `gitleaks:allow` comments are ignored. A committed `.gitleaksignore` fails the
     gate (gitleaks would read one at the root of the folder it scans), and so does one in git's own folder.
     The commit and message scans run from an empty folder, because for the message scan gitleaks reads one in
     the working folder too.
   * Findings print with the secret redacted.
2. **Native-code audit** (the "100% Rust" ruling, D1): `scripts/audit_native.py --check`.
   * It finds every Cargo workspace in the export by itself, so new crates and new workspaces are covered as they
     appear. A crate that is in no audited workspace fails.
   * A package with its own `Cargo.lock` is a root of its own only if no audited workspace already covers it (a
     vendored crate may ship a lock).
   * For each workspace and each target in the allow file, it asks `cargo tree` which packages are built. The
     edges come from `cargo tree`, the real feature resolver, not from `cargo metadata`. The old audit took them
     from metadata, and that made 3 of its 14 Windows flags false: pkg-config, and khronos-egl's.
   * It classifies every package, including the workspace's own crates (see the script's docstring for the
     classes). A prebuilt binary is found by its extension, or by its first bytes whatever its name (ELF, PE,
     Mach-O, ar archives, wasm modules). A build script counts as linking when it, or a file beside or below it,
     emits `rustc-link-lib` or `rustc-link-arg`.
   * Every flag needs a line in `scripts/native_allow.txt`, with its reason. Lines can require features to be on
     or off. The audit checks those both for the workspace as a whole and for each crate built on its own:
     `cargo build -p` unifies features over that crate's graph only. For example, blake3 is accepted only with
     `pure`.
   * A committed `.cargo/config.toml` can name a program for cargo to run, and `rust-toolchain.toml` can name a
     toolchain. So the audit runs with the base's versions of those files, and the gate lists any the landing
     changes.
3. **Publication check:** `scripts/publish_check.sh`, the term check that the publishing step already runs.
   * It needs this clone's term file and allowlist: `git config pe.gate.terms <file>` and `pe.gate.allow <file>`
     (or `PE_PUBLISH_TERMS` and `PE_PUBLISH_ALLOW`).
   * It checks the tree of `<rev>`, and the added lines and messages of `<base>..<rev>`. Every term blocks there.
     Identities are left to the publishing step, which maps them.
   * The terms are never published, so hosted CI skips this check.
   * A `--full-history` run doesn't fail on the old history's terms: the user chose to keep that history
     (2026-10-31 11:49Z), and the cleaner replaces those terms.

On main at 7155e8f (after M0's archive, the seams and the first M1 crates), the audit checks 7 workspaces and 681
package builds, and every flag is accepted. `archive/` is excluded: the prototype's crates are kept there to read and port from, in no workspace,
and are never built. The secret scan of the whole history finds nothing.

## Running it

* **Before a landing:** `bash scripts/gate.sh --base main <branch>`, with `PE_GATE_WRAP='bash "<team folder>/heavy.sh"'`
  so that the audit's cargo calls take a build slot. It takes about half a minute, most of it `cargo metadata`
  and `cargo tree`; nothing is built. Run it in any checkout: it hands over to main's copy of the gate.
* **Install gitleaks once per clone:** `bash scripts/install_gitleaks.sh`. It puts the pinned binary, checked
  against the SHA-256 of its archive and of the binary itself, in `.tools/` at the root of the main checkout,
  which is git-ignored. To move to a newer gitleaks, change the version and all the pins together, and run
  `--full-history` in the same change. The new version applies from the next landing.
* **The hook,** `scripts/hooks/reference-transaction`, refuses any update of main whose new commit fails the
  gate: a merge, a commit, a reset, `update-ref`, or a fetch or push into the repository. Other ref updates pass
  through at once, and so does one that leaves main where it is (a reset to HEAD, `merge --abort`, a stash).
  It refuses to delete main (`branch -D`, `update-ref -d`, renaming it), except when `pack-refs` (run by
  `git gc`) drops main's loose file after writing main to `packed-refs`, which keeps main's tip. It also refuses
  a move it can't judge: main's tip can't be read, though git gives an old value for it or main is there (a
  loose file or a packed line).
  * Enable it once per clone: `bash scripts/install_gate_hook.sh`. It writes a small trampoline into git's own
    folder (`.git/pe-gate-hooks/`), outside every working tree, and points `core.hooksPath` at it. The trampoline
    runs the hook as committed at main's old tip, and the hook runs that tip's gate. So no landing can change or
    delete the hook that judges it. The old tip is read from main as git holds it locked: for a forced update
    (`update-ref` without an old value, `branch -f`) git gives the hook zeros as the old value.
  * The installer runs only once the gate is on main, since the trampoline refuses every update of main while
    main's tip has no hook. It and the gate need git 2.40 or later: an older git ignores `GIT_ATTR_SOURCE`.
  * Two ways round it, for emergencies only. Whoever uses either reports it to the coordinator in the same turn:
    * `git config --unset core.hooksPath` turns the hook off for the clone and all its worktrees; the installer
      turns it back on;
    * `PE_GATE=off` before one git command lets that command through unchecked, a deletion of main included.
  * The hook holds main's lock while the gate runs, so it must never wait for a build slot: the main clone has
    `git config pe.gate.wrap "nice -n 19"` (idle priority, no slot; the audit builds nothing). Run the check
    before merging under the build-slot wrapper instead (`PE_GATE_WRAP='bash "<team folder>/heavy.sh"'`).
  * The hook, its trampoline and the gate put their temporary files (the gate exports the whole tree) in the
    folder `git config pe.gate.tmpdir <folder>` names, when that is an absolute path to a folder that exists (on
    Windows written `S:/like/this`), instead of `TMPDIR`'s. The main clone keeps them off C: this way.
  * Give the merge a long tool timeout, and never wrap it in `timeout`: that kills git but not the hook, and
    leaves the checkout half-applied. A git killed by other means leaves `.git/HEAD.lock` and
    `.git/refs/heads/main.lock` behind; remove them only once no git process runs in the clone.
  * When it refuses a merge in the main checkout, main is unchanged. `git merge --abort` puts the checkout back;
    after a fast-forward or a reset, use `git reset --merge HEAD`.
* **Hosted CI,** `.github/workflows/gate.yml`, runs the gate on every push to main, every pull request, and
  nightly over the whole history (user ruling Q12). It installs gitleaks and runs the gate from the base too.
  GitHub runs the workflow file as the landing has it, so the gate's NOTE lists a change to it.
* **The self-test,** `bash scripts/test_gate.sh`, takes about 20 minutes; run it as one build-slot job
  (`bash "<team folder>/heavy.sh" bash scripts/test_gate.sh`). In a throwaway clone it runs the gate as the hook
  does, in 35 checks:
  * secrets in eleven ways: in the tree, only in the history, only in a message, behind an inline
    `gitleaks:allow`, a pe-app member key file, under an `export-ignore` path, in a file marked `-diff`, added
    only by a merge, behind a committed `.gitleaksignore` or one in the folder the gate runs from, and a session
    key that isn't one of the recorded values;
  * native code in seven ways: a crate of the workspace that compiles C, blake3 without `pure` in one crate, a
    crate in no workspace, a new workspace, a workspace under an `export-ignore` path, a build script whose
    module emits `rustc-link-arg`, and an executable without an extension;
  * a publication term in three ways: in the tree, only in the history, and in a file marked `-diff`;
  * the gate's own code: a landing that changes it is judged by the base's, one that runs its own copy hands
    over, one that adds its own exception has it applied, a committed gitleaks binary is refused, a committed
    cargo configuration isn't applied, and the export refuses a symbolic link, a submodule and a case twin;
  * the hook: the installer waits until main has the gate; the hook refuses a merge and an update-ref, lets a
    clean merge in, runs the base's hook and gate when a landing empties both, also when it comes as a forced
    update, and returns at once from a reset to HEAD, `reset --merge`, a same-value `update-ref` and a
    checkout that moves only HEAD, while the audit's wrapper stalls; it refuses to delete main (and lets
    `PE_GATE=off` do it), lets `pack-refs` pack main, and refuses a move when main's tip can't be read.

  Run it after changing the gate.

## Tier 1: checks by the paths a landing touches

Some checks matter only when a landing touches certain paths, and compile, which the gate never does: it runs
inside main's lock. `scripts/tier1_checks.txt` lists them, each with its paths (git pathspecs with glob magic),
one command, and optionally a regex its output must match (`expect`: a test filter that matches no test exits 0,
so a renamed or ignored test would otherwise pass). `scripts/tier1.sh` runs the ones a landing triggers.

* **Integrators run it** on the commit they hand in, after the rebase, from the root of a clean checkout of it,
  under the build-slot wrapper, and put its output in the hand-in's evidence:
  `PE_TIER1_WRAP='bash "<team folder>/heavy.sh"' bash scripts/tier1.sh --base main --run <commit>`.
  Without `--run` it lists the checks the landing triggers and the paths that trigger them. `--run` refuses
  uncommitted changes, and untracked files under a triggered check's paths, which would be tested in place of
  the landing's.
* **`{crates}` scopes a check to the crates a landing touches** (refound plan 4.6). In a command it becomes
  `-p <package>` for each workspace member (the root `Cargo.toml`'s members, as the landing has them) whose
  folder holds a path that triggers the check. A path on the check's `all` lines (the toolchain pin, a cargo
  configuration, the root manifest and `Cargo.lock`, for clippy also a clippy configuration) makes it every
  member instead. The check's
  `skip` packages are never included (box3d-fork, whose package is `box3d-rust`, by C10). When none is left, the
  check has nothing to run, which isn't a failure. A landing that drops a crate from the members while it
  changes files in the crate's folder fails, since no check would build them (removing the member together
  with its folder is fine). A touched crate's dependents aren't tier 1's: the job's workspace check compiles
  them, and tier 2 tests them.
* **No crate is exempt.** The coordinator's ruling allowed an exempt list that only shrinks, for
  crates that failed clippy when the check started. The census on main ff9be64b found none: every one of the 19
  crates passed `clippy --no-deps -- -D warnings` on its own, with no warning. So the table has no exempt list,
  and the runner has no way to write one: a landing whose own table is malformed (an unknown keyword included)
  fails tier 1, since that table would stop every landing after it.
* **A check runs the landing's own code** (its test, and the files the test reads), so a landing that changes
  what a check runs changes the check that judges it. Read such a diff as a change to the check.
* **The table and the runner come from the base,** like the gate's code (the runner hands over to the base's
  copy), so a landing can't drop or skip a check it triggers, and a change to either applies from the next
  landing. While the base has no table, the landing's own applies, with a NOTE. A change to them is a gate
  change, with its review.
* **Today's checks:**
  * `lamp-bytes` (coordinator rulings): a script module's fuel counts follow its
    bytes. A landing that touches pe-logic, pe-sdk, the toolchain pin, a cargo configuration file or Cargo.lock
    builds the lamp test script in two folders and again from a warm cache, and checks that the modules are the
    same bytes. It needs the `wasm32-unknown-unknown` target. The same check across two operating systems isn't
    tier 1 but periodic: tier 2's hosted matrix after 2026-11-16, and until then a run on WSL after a landing.
  * `fmt`: `cargo fmt --all --check`, when a Rust file or a rustfmt configuration changes. It builds nothing.
  * `clippy`: `cargo clippy {crates} --all-targets --locked --no-deps -- -D warnings`. `--no-deps` keeps it to
    the touched crates: without it, clippy also lints the workspace members they depend on.
  * `test`: `cargo test {crates} --locked`, the touched crates' unit, integration and doc tests, with default
    features; every crate's when the root manifest (its profiles touch determinism), `Cargo.lock` (a dependency
    bump changes behaviour), the toolchain pin or a cargo configuration changes. Every crate's tests take 542 s
    warm, plus up to 609 s to rebuild the test binaries (main 4bbc52ea, 3 jobs, idle priority).
  * `model-fast`: the agreement model's fast profile (`models/cuts/README.md`, about 2 minutes), when
    `models/cuts/` changes. Its output must say that all 16 checks met their expectation.
  * The harness's fast profile follows once its owner names the command.
* **Its self-test,** `bash scripts/test_tier1.sh`, runs no cargo, so it needs no build slot. In a throwaway clone
  it checks, in twenty-two cases, that a landing triggers and runs the checks its paths name and no others, that
  the table and the runner come from the base, that a failing command fails the run and so does one that exits 0
  without its expected output, that `--run` refuses a checkout that isn't at the landing or isn't clean where it
  matters, and that a malformed table is an error. For `{crates}`, it checks that it holds exactly the touched
  members less the skipped ones (every member for an `all` path), that a landing which drops a member while
  changing its files fails and one which removes it with its folder doesn't, and that a landing whose own table
  is malformed fails.
* Hosted CI will run it too, later (refound-plan 4.6).

## Accepting an exception

The allow files come from the landing itself, so a landing can accept its own finding, and the gate's NOTE is
what shows it. **A landing report quotes the gate's NOTE lines**, and the landing's reviewer approves every line
they name.

* **Native code:** add an `allow` line (or a `class`, `exclude` or `targets` line) to `scripts/native_allow.txt`.
  Give the reason, with evidence: the build script's lines, and the features it depends on.
* **A false secret:** add an `[[allowlists]]` entry to `.gitleaks.toml` that matches the exact line, and its path,
  with the reason. The gate takes only `[[allowlists]]` entries from a landing, one key per line, with a string
  or an array of strings (`description`, `targetRules`, `condition`, `paths`, `regexTarget`, `regexes`,
  `stopwords`, `commits`).
* **A line that only looks like a term:** add it to the publication allowlist by its path and line hash
  (`scripts/publish_allow.py`). The allowlist names the line, never the term.

## What it can't see

The audit is a static reading of the build graph and of the files in each package. It doesn't catch:
* a build script that downloads a prebuilt library, or runs a compiler on a file it ships under a name that isn't
  a C, C++, assembly or Objective-C extension, with no helper crate;
* a build script that includes a file from outside its own folder (`include!("../x.rs")`) to emit link directives;
* a procedural macro that runs a compiler;
* `#[link]` attributes in dependencies (OS bindings such as windows-sys and libc have them, so they aren't
  flagged). In the workspace's own crates they are flagged.

The secret scan matches patterns and entropy, so a credential in a new format can pass. The term check
(`scripts/publish_check.sh`) covers names and addresses before each public push.
