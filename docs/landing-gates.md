# The landing gate

One command, `scripts/gate.sh`, checks a commit before it lands on main. It runs the native-code audit with the
feature policy for shipped builds, the secret scan, the publication check where the term files are configured, and,
where the clone asks for it, tier 2's state. The re-founding plan (`docs/refound-plan.md`, 4.6) adds two tiers:
tier 1 runs the format, lint, test and model checks by the paths a landing touches, and tier 2 runs every crate's
tests on four hosted platforms after each landing. Owner: WS-ci.

## What it checks

`bash scripts/gate.sh [--base <rev>] [--full-history] [--report <folder>] [<rev>]`

It checks `<rev>` (default HEAD) as it would land on `--base` (default main). It exits 0 on a pass, 1 on a
fail and 2 on an error.

**The gate never judges a change to itself with the changed code** (coordinator ruling, 2026-11-10):
* When there is a base, the gate's code comes from it: `scripts/gate.sh` (a copy run from the landing hands over
  to the base's at once), `scripts/audit_native.py`, `scripts/install_gitleaks.sh`, the hook, the publication
  check's scripts, `scripts/ci_status.py`, the rules of `.gitleaks.toml`, the feature policy
  (`scripts/feature_policy.txt`), and the cargo and rustup configuration files the audit runs with.
* Only the allow files come from `<rev>`: `scripts/native_allow.txt`, the `[[allowlists]]` entries of
  `.gitleaks.toml`, and `scripts/tier2_explained.txt`.
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
   * **The feature policy** (`scripts/feature_policy.txt`, from the base): no shipped binary may build a forbidden
     feature. The file lists the shipped crates with their targets, and the forbidden features, each with its
     reason. Each shipped crate is checked as built on its own, `cargo tree -p <crate> -e normal` with features,
     on each of its targets. Shipped binaries are built per crate (`cargo build -p pe-app --release`). A
     workspace-wide build unifies features across every member it builds, so a feature the harness turns on
     would reach them. A shipped crate that no workspace has yet gets a note.
     A landing that changes the policy must also meet the new one, over its own tree, since the landings after
     it are judged by it. A policy that doesn't parse (a crate with two `shipped` lines counts) or that names a
     target cargo can't resolve would stop all of them, its fix included, and one the tree already breaks would
     fail each of them until a fix lands. A landing that deletes the file fails too: to retire the policy, change
     the gate's code first, or land with `PE_GATE=off` and report it.
3. **Publication check:** `scripts/publish_check.sh`, the term check that the publishing step already runs.
   * It needs this clone's term file and allowlist: `git config pe.gate.terms <file>` and `pe.gate.allow <file>`
     (or `PE_PUBLISH_TERMS` and `PE_PUBLISH_ALLOW`).
   * It checks the tree of `<rev>`, and the added lines and messages of `<base>..<rev>`. Every term blocks there.
     Identities are left to the publishing step, which maps them.
   * The terms are never published, so hosted CI skips this check.
   * A `--full-history` run doesn't fail on the old history's terms: the user chose to keep that history
     (2026-10-31 11:49Z), and the cleaner replaces those terms.
4. **Tier 2's state** (decision C7), where the clone asks for it with `git config pe.gate.tier2`: the latest
   finished hosted run on the public main, read through `scripts/ci_status.py --check` within 20 s. A red run
   fails the gate until it is fixed or explained (below, "Tier 2: the hosted platforms").

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
* **Hosted CI,** `.github/workflows/gate.yml`, runs on every push to main, every pull request, and nightly (user
  ruling Q12). GitHub runs the workflow file as the landing has it, so the gate's NOTE lists a change to it.
  * The gate, and nightly over the whole history. It installs gitleaks and runs the gate from the base too.
  * Tier 2's jobs, on four platforms (below, "Tier 2: the hosted platforms").
* **The self-test,** `bash scripts/test_gate.sh`, takes about 50 minutes. Its cargo calls are `cargo metadata` and
  `cargo tree`, which build nothing, so it runs without a build slot, under the hook's idle-priority wrapper in
  `PE_GATE_WRAP` (coordinator m7542529cb084). In a throwaway clone it runs the gate as the hook does, in 69
  checks:
  * secrets in eleven ways: in the tree, only in the history, only in a message, behind an inline
    `gitleaks:allow`, a pe-app member key file, under an `export-ignore` path, in a file marked `-diff`, added
    only by a merge, behind a committed `.gitleaksignore` or one in the folder the gate runs from, and a session
    key that isn't one of the recorded values;
  * native code in seven ways: a crate of the workspace that compiles C, blake3 without `pure` in one crate, a
    crate in no workspace, a new workspace, a workspace under an `export-ignore` path, a build script whose
    module emits `rustc-link-arg`, and an executable without an extension;
  * a publication term in three ways: in the tree, only in the history, and in a file marked `-diff`;
  * the feature policy, on a test base whose policy names planted crates: a harness that turns forbidden features
    on for its own build passes, and a shipped crate that turns on a forbidden feature or a `mutants` feature
    fails, even when the landing also drops the policy's line. A landing that changes the policy fails when its
    own policy doesn't parse (a crate with two `shipped` lines), names a target cargo can't resolve, or is
    broken by its own tree, even where the base's policy allows it, and one that deletes the policy fails;
  * tier 2's state, from files standing in for GitHub: a red run fails the gate with `pe.gate.tier2` on, passes
    once `scripts/tier2_explained.txt` names it, and is only reported with `note`; a run cancelled before any job
    started is looked past, and a fork's pull request from its own `main` counts for nothing, green or red; a
    cancelled run with a failed job, or with a job that started and was cancelled, is red, and a platform never
    given a runner leaves its run with no verdict, even one GitHub calls a failure, and six such runs in a row
    leave tier 2's state unknown; an unreadable answer, a malformed one and an unknown mode are each handled as
    stated below; and a server that never finishes its answer is cut off at 20 s, after which the gate passes
    with a NOTE. For triage: a red run is named once, with the commits a platform back from runs without a runner
    left unverified, and a platform with no real verdict for 3 runs in a row, or for 24 h, is reported once. The
    digest comparison notes a platform that didn't run, lets the first that ran stand in for windows-x64, and
    fails on a platform without digests or a line that differs. A failed step's annotation carries a failed
    test's panic but not a should_panic test's, and what a failed linker said, each line once, without its
    command line;
  * the gate's own code: a landing that changes it is judged by the base's, one that runs its own copy hands
    over, one that adds its own exception has it applied, a committed gitleaks binary is refused, a committed
    cargo configuration isn't applied, the export refuses a symbolic link, a submodule and a case twin, and the
    gate's temporary files go to the folder `pe.gate.tmpdir` names, run directly and through the trampoline;
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
* **No crate is exempt.** The coordinator's ruling (ma3d5082e0516) allowed an exempt list that only shrinks, for
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
  * `lamp-bytes` (coordinator rulings mcda7c814aebf, m721068896f96): a script module's fuel counts follow its
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
* Tier 2 runs every crate's tests and clippy on four hosted platforms after each landing (below).

## Tier 2: the hosted platforms

Tier 2 runs on GitHub's hosted runners (user ruling Q12), from `.github/workflows/gate.yml`, after each push to the
public repository's main and nightly (refound plan 4.6). The public main is main as the publishing step pushes it,
and its cleaning leaves `crates/`, `models/` and `scripts/` byte for byte as they are. So tier 2 judges what has
already landed, and a red run holds up the landings after it (decision C7).

* **On each platform** (windows-x64, linux-x64, linux-arm64 and macos-arm64), `scripts/tier2.sh` runs:
  * the kept determinism base's tests (box3d-rust, pe-core, pe-sim, pe-physics), in release and in debug;
  * every other crate's tests;
  * clippy with warnings denied, on every crate but box3d-rust.

  pe-physics's tests compare goldens, traces and snapshot images with the committed records, which were made on
  Windows, so a pass means bit-identical results. The script runs the same way on a developer's machine:
  `bash scripts/tier2.sh <out-folder>`.
* **The digests.** Each platform keeps the RESULT lines its tests printed, per profile. A final job
  (`scripts/tier2_digests.py`) requires them to be the same as windows-x64's, line for line. A platform that kept
  nothing didn't run: it is noted and not compared (if windows-x64 didn't run, the first platform that did stands
  in), and its own job decides what that means for the run (below).
  * **A RESULT line is a contract** (coordinator m5fa7906e0a43). A test prints one only for a value that must be the
    same on every platform, never a timing or anything else that depends on the machine.
  * A workstream adds a cross-platform digest by printing a RESULT line from a test (fuel and trap points,
    replica replays, randsched roots). The workflow doesn't change.
* **Nightly,** the agreement model's checks run too (`models/cuts`): the fast profile, the bounded n = 4 Byzantine
  checks and the backoff checks, 25 in all. The model's owner chooses the set.
* **A red run blocks the next landing** until it is fixed or explained (decision C7). The gate's step 5 reads the
  latest finished run on the public main (`scripts/ci_status.py --check`, from the base). `git config
  pe.gate.tier2` (or `PE_GATE_TIER2`) sets what the gate does with it:
  * `on`: a red run fails the gate, unless `scripts/tier2_explained.txt` lists it. The landing that fixes it, or
    explains it, adds `<run id> -- <reason>` there. It is an allow file, read from the landing, and its reviewer
    approves the line;
  * `note`: the gate only reports a red run. The main clone stays here until every platform has run once and
    the red jobs of the first four-platform run are fixed and published, or explained (coordinator
    m5fa7906e0a43, m7b238a22d869);
  * `off`, the default: no check. Any other value is an error.
  * Only main's own runs count: a push, the nightly run or a manual dispatch, in the public repository itself.
    A pull request's run never does, not even one from a fork's own branch called `main` (GitHub's branch filter
    matches that too).
  * **A run's jobs give its verdict** (coordinator m4445bf31f965). It is red when a job failed or timed out, or
    started and was then cancelled: GitHub reports a job stopped at its time limit as cancelled, like one
    cancelled by hand. Failing that, a platform GitHub never gave a runner ("not acquired": it had no capacity,
    as for macos-arm64 in the first run) is "not run". Its result is unknown, so the run has no verdict, whatever
    GitHub calls it. So has a run cancelled before any job started (a burst of pushes cancels the runs still
    waiting), and a skipped or stale one. The check looks past a run with no verdict to the run before, and names
    the platforms that didn't run. It judges 5 runs at most, since each costs one of the 60 API calls an hour this
    machine may make. If none of them gives a verdict, tier 2's state is unknown, and the gate says so and passes
    (chunks-reviewer's L1).
  * The check gives up after 20 s in all, because the hook runs it while git holds main's locks. If GitHub can't
    be read in time, or its answer can't be read, the gate says so and passes.
  * **The check keeps each finished run's verdict** in `pe-tier2-verdicts.json` in the clone's git folder, which
    its worktrees share, under the run's id and attempt (coordinator m9f85508b37fe). A finished attempt never
    changes, and a re-run is a new attempt, judged afresh. A check then costs one call, the listing, until a new
    run finishes. Before, each of a landing's checks (the lander's gate, the hook, the integrator's run) cost two
    to four calls, and on 2026-11-17 a busy hour spent all 60: GitHub refused with HTTP 403, and an answer of 304
    counts too without a login. A cache that can't be read or written only costs calls. If the 60 are spent
    anyway, the gate says GitHub can't be read and passes; a red run blocks again once GitHub answers.
  * **A read-only token lifts the 60** (the user's ruling, coordinator m1434ef6522c4). The 60 are this address's,
    shared with everything else on the machine, and on 2026-11-18 they ran out twice in five hours with the cache,
    each time leaving the gate unable to block. `git config pe.gate.ghtoken <file>` (or `PE_GATE_GHTOKEN`) names a
    file outside any repository that holds a GitHub token: a fine-grained one with public repositories
    (read-only) and no permissions. The check then reads with it, and its calls count against the token's 5,000
    an hour. The token goes only to `https://api.github.com`, never across a redirect, and is never printed. A
    missing or empty file means no token, as before; a file holding anything else is noted on the gate's tier-2
    line. If GitHub refuses the token (revoked or expired: HTTP 401 or 403), the check makes the call again
    without it and its line says so. To read with it by hand too:
    `python -I scripts/ci_status.py --token-file "$(git config --get pe.gate.ghtoken)" --run <id>`.
* **Triage** is WS-ci's.
  * A watchdog runs `scripts/ci_status.py --new <state file>` every 30 minutes. It names each red run once. When
    a platform is back in a red run after runs without a runner, the line adds the commits it left unverified:
    those since its last real verdict. When a platform has had no real verdict for 24 h or for 3 runs in a row, it
    reports `<platform> unverified since <sha>` once, and WS-ci passes that to the coordinator. Neither blocks a
    landing (coordinator m4445bf31f965).
  * `scripts/ci_status.py --run <id>` gives a run's verdict and lists the jobs that didn't pass, with their
    failing tests and each failed test's panic message, what a failed linker said (rustc's notes, without the
    linker's command line), and any differing digests. A step's errors come once each, however many tests hit
    them. The jobs write these as error annotations, because GitHub serves those without a login and logs need one
    (to its admins only). GitHub keeps an annotation's first 4096 characters, so what the linker said comes first,
    also when a test printed the build error with `{:?}`, its lines escaped on one; then the errors, which name
    every failed test binary; then the failing tests and their panics, each line at most 300 characters.
  * A run's commit is the public one: the publishing step's map leads back to main's.

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
