//! pe-diverge on the simulation thread (app.md 3, steps 1 and 5; docs/spec/divergence.md).
//!
//! The loop owns a [`Diverge`] and what pe-diverge leaves to its embedding:
//! - the cuts agreement asked a root for, kept by `CutRef` until a cut past them is final, because a `RootSplit`
//!   names only the cut's `CutRef` and pe-diverge replays to the whole `Cut` (its heads);
//! - step 5's one ordered list: each `finalize` result right after its cut's `CutFinal` (first if that `CutFinal` came
//!   in an earlier tick), and `RootSplit`s in the order S3 queued them;
//! - its actions: revotes go to [`Agreement::vote`] with the current verdict, a self-test runs here, and a fault
//!   bundle goes to the I/O thread's bounded queue, never written on this thread;
//! - the physics self-test: the digest of pe-physics's fixed scene, taken at start-up and again on each
//!   `RunSelfTest`. A different digest is `Failed`; the same is `Passed`. A vote never claims a test that did not run.
//!   Once `--inject-fault-at`'s fault has fired on this machine, the verdict is `Failed` by injection whatever the
//!   digest ([`crate::fault`]): the notice and every bundle's `build` line say so, beside the real digest's verdict.
//!   In M1 it runs on the simulation thread while it takes under a frame on M1's machines (app.md 1), and the
//!   start-up digest is the value R-G1's hello will carry. Both are taken on the simulation thread (the start-up one
//!   at the first step), so both see that thread's floating-point mode, which the digest depends on (@vdw-physics).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

use pe_diverge::{Action, Config, Counters, Diverge, FaultBundle};
use pe_physics::SelfTestDigest;
use pe_seams::agree::Fault;
use pe_seams::{
    AgreeEvent, Agreement, Cut, CutRef, Finalized, FrameClock, HostTime, MemberKey, Outgoing, Replica, SelfTest,
    VoteResult,
};

use crate::fault::Fault as InjectedFault;
use crate::sim::SimStats;

/// Fault bundles waiting for the I/O thread: past this many, a bundle is dropped and counted, never waited for.
pub const BUNDLE_QUEUE: usize = 8;

/// The most cuts kept for a later `RootSplit`. Agreement asks roots for a handful of cuts above the final one; this
/// only bounds a run whose finality stopped for good.
const CUTS_KEPT: usize = 256;

/// The fault-bundle folder of the machine whose identity seed is the file `identity`: `<its name>.bundles` beside it,
/// as the build folder ([`crate::builds::build_dir`]), so two instances on one PC never write one bundle name twice
/// (both members' bundles for one split share a name).
pub fn bundle_dir(identity: &std::path::Path) -> PathBuf {
    let mut name = identity.file_name().map_or_else(|| "identity".into(), |n| n.to_os_string());
    name.push(".bundles");
    identity.with_file_name(name)
}

/// A bundle's `build` line (FaultBundle's doc): the build id the hellos compare (pe-app's version, the script build
/// and the runner's identity, [`crate::net::build_id`]), the platform, and the CPU features a machine's code could
/// branch on. pe-physics takes none of them (its fork runs one scalar path, @vdw-physics), so they only tell two
/// machines apart in a bundle.
fn build_line() -> String {
    let id = String::from_utf8_lossy(crate::net::build_id()).into_owned();
    format!("{id}; {}-{}; cpu {}", std::env::consts::OS, std::env::consts::ARCH, cpu_features())
}

/// The x86-64 features in use or in reach; elsewhere, none named.
fn cpu_features() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        let known = [
            ("sse4.2", std::arch::is_x86_feature_detected!("sse4.2")),
            ("avx", std::arch::is_x86_feature_detected!("avx")),
            ("avx2", std::arch::is_x86_feature_detected!("avx2")),
            ("fma", std::arch::is_x86_feature_detected!("fma")),
            ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
        ];
        let on: Vec<&str> = known.iter().filter(|(_, on)| *on).map(|(n, _)| *n).collect();
        on.join(",")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        "n/a".to_string()
    }
}

/// The self-test's verdict for `split`: `Passed` when the digest now equals start-up's, `Failed` otherwise, and
/// `Failed` when either run could not complete, since nothing then vouches for this machine's physics.
pub fn verdict(
    startup: &Result<SelfTestDigest, String>,
    now: &Result<SelfTestDigest, String>,
    split: CutRef,
) -> SelfTest {
    match (startup, now) {
        (Ok(a), Ok(b)) if a.digest == b.digest => SelfTest::Passed { split },
        _ => SelfTest::Failed { split },
    }
}

/// The console's line for a self-test that found the thread's floating-point mode other than the default, and reset it
/// first (pe-physics `SelfTestDigest::mode_reset`; @coordinator-opus): the digest is still the default
/// mode's, but something on this thread changed the mode.
fn mode_notice(which: &str) -> String {
    format!(
        "divergence: the floating-point mode was not the default when the {which} self-test ran; it was reset for it, \
         so the digest is the default mode's (a library on the simulation thread changed the mode)"
    )
}

/// The console's line for a self-test that the injected fault fails ([`crate::fault`]): it names the injection and
/// the real digest's verdict, so nobody reads it as a physics fault.
fn injected_notice(cut: CutRef, f: &InjectedFault, real_failed: bool) -> String {
    let real = if real_failed { "failed too" } else { "passed" };
    format!(
        "divergence: the physics self-test for the split at {cut:?} reports Failed BY INJECTION ({}, a deliberate test \
         fault, not a physics fault); the real self-test {real}",
        f.source()
    )
}

/// pe-physics's self-test, in the default floating-point mode, with its error as text.
pub fn self_test_digest() -> Result<SelfTestDigest, String> {
    pe_physics::self_test_digest().map_err(|e| format!("{e:?}"))
}

/// pe-diverge's state on the simulation thread, with the I/O thread it writes bundles through.
pub(crate) struct DivergeLoop {
    pub(crate) d: Diverge,
    /// The self-test digest taken at start-up: at the first step, on the simulation thread.
    startup: Option<Result<SelfTestDigest, String>>,
    /// The cuts agreement asked a root for, above the final cut.
    cuts: BTreeMap<CutRef, Cut>,
    /// The bundles' `build` line ([`build_line`]), computed for the first bundle written.
    build: Option<String>,
    io: Option<BundleWriter>,
}

impl DivergeLoop {
    /// `bundles` is the folder fault bundles are written to, by a `pe-io` thread started here; `None` writes none
    /// (each is counted as dropped and logged).
    pub(crate) fn new(me: MemberKey, cfg: Config, bundles: Option<PathBuf>) -> std::io::Result<DivergeLoop> {
        let io = bundles.map(BundleWriter::start).transpose()?;
        Ok(DivergeLoop { d: Diverge::new(me, cfg), startup: None, cuts: BTreeMap::new(), build: None, io })
    }

    /// The start-up digest, taken now if not yet.
    fn startup(&mut self) -> &Result<SelfTestDigest, String> {
        self.startup.get_or_insert_with(self_test_digest)
    }

    /// Replaces the start-up digest: a test's way to make the next self-test differ.
    #[cfg(test)]
    pub(crate) fn set_startup_digest(&mut self, digest: Result<SelfTestDigest, String>) {
        self.startup = Some(digest);
    }

    pub(crate) fn counters(&self) -> &Counters {
        self.d.counters()
    }

    /// Before step 4's `drive`: the cuts agreement wants roots for now, the list the driver walks.
    pub(crate) fn note_cuts(&mut self, agree: &dyn Agreement) {
        for c in agree.wants_roots() {
            self.cuts.entry(c.at).or_insert(c);
        }
        while self.cuts.len() > CUTS_KEPT {
            self.cuts.pop_first();
        }
    }

    /// Step 1: a Repair-lane message.
    pub(crate) fn receive(
        &mut self,
        from: MemberKey,
        bytes: &[u8],
        replica: &mut dyn Replica,
        agree: &mut dyn Agreement,
        now: HostTime,
        clock: &dyn FrameClock,
    ) -> Result<(), Fault> {
        self.d.receive(from, bytes, replica, agree, now, clock)
    }

    /// Step 5: S3's events drained this tick and the driver's `finalize` results, as one ordered list; then
    /// pe-diverge's time, and its actions. Returns the Repair-lane messages to send.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn step(
        &mut self,
        events: &[AgreeEvent],
        finalized: &[(CutRef, Finalized)],
        replica: &mut dyn Replica,
        agree: &mut dyn Agreement,
        now: HostTime,
        clock: &dyn FrameClock,
        self_test: &mut SelfTest,
        injected: Option<&InjectedFault>,
        stats: &mut SimStats,
        notices: &mut Vec<String>,
    ) -> Vec<Outgoing> {
        // `--inject-fault-at`'s fault, once it has fired here: every self-test from then on reports `Failed`.
        let injected = injected.filter(|f| f.fired());
        let first = self.startup.is_none();
        if self.startup().as_ref().is_ok_and(|d| d.mode_reset) && first {
            stats.self_test_mode_resets += 1;
            notices.push(mode_notice("start-up"));
        }
        // `NotReady` is the driver's: it asks `finalize` again on the next tick.
        let results: Vec<(CutRef, Finalized)> =
            finalized.iter().filter(|(_, f)| !matches!(f, Finalized::NotReady)).cloned().collect();
        let fc = agree.final_cut().cloned();
        let final_frame = fc.as_ref().map(|f| f.cut.at.frame);
        // The driver finalizes only `final_cut()`, so a result is always for it.
        let on_final =
            |d: &mut Diverge, cut: &CutRef, f: Finalized, replica: &mut dyn Replica, agree: &mut dyn Agreement| {
                match &fc {
                    Some(fc) if fc.cut.at == *cut => d.on_final(fc, f, replica, agree, now, clock),
                    _ => d.on_cut_final(*cut),
                }
            };
        let in_list: BTreeSet<CutRef> = events
            .iter()
            .filter_map(|e| match e {
                AgreeEvent::CutFinal { cut, .. } => Some(*cut),
                _ => None,
            })
            .collect();
        for (cut, f) in results.iter().filter(|(c, _)| !in_list.contains(c)) {
            on_final(&mut self.d, cut, *f, replica, agree);
        }
        for e in events {
            match e {
                AgreeEvent::CutFinal { cut, .. } => match results.iter().find(|(c, _)| c == cut) {
                    Some((_, f)) => on_final(&mut self.d, cut, *f, replica, agree),
                    None => self.d.on_cut_final(*cut),
                },
                // A split for a cut at or below the final cut's frame is settled already (pe-app's split set skips
                // it too).
                AgreeEvent::RootSplit { cut, .. } if final_frame.is_some_and(|f| cut.frame <= f) => {}
                AgreeEvent::RootSplit { cut, roots } => match self.cuts.get(cut) {
                    Some(c) => {
                        let c = c.clone();
                        self.d.on_root_split(&c, roots, replica, agree, now, clock);
                    }
                    None => {
                        stats.splits_unknown += 1;
                        notices.push(format!("divergence: a root split at {cut:?}, a cut this machine never voted on"));
                    }
                },
                _ => {}
            }
        }
        if let Some(f) = final_frame {
            self.cuts.retain(|c, _| c.frame > f);
        }
        let out = self.d.tick(now, clock, replica, agree);
        // What pe-diverge did, for the log: a handful per final cut while a machine diverges, none while healthy.
        notices.extend(self.d.events().into_iter().map(|e| format!("divergence: {e:?}")));
        notices.extend(self.d.notes().into_iter().map(|n| format!("divergence: {n}")));
        for a in self.d.actions() {
            match a {
                Action::Revote { cut, root } => match agree.vote(&cut, root, *self_test) {
                    VoteResult::Voted | VoteResult::AlreadyVotedThisView => stats.revotes += 1,
                    _ => stats.revotes_refused += 1,
                },
                Action::RunSelfTest { cut } => {
                    let now = self_test_digest();
                    if now.as_ref().is_ok_and(|d| d.mode_reset) {
                        stats.self_test_mode_resets += 1;
                        notices.push(mode_notice(&format!("split at {cut:?}'s")));
                    }
                    let real = verdict(self.startup(), &now, cut);
                    stats.self_tests += 1;
                    let failed = matches!(real, SelfTest::Failed { .. });
                    if failed {
                        stats.self_tests_failed += 1;
                        notices.push(format!("divergence: the physics self-test for the split at {cut:?} failed"));
                    }
                    *self_test = real;
                    if let Some(f) = injected {
                        stats.self_tests_injected += 1;
                        *self_test = SelfTest::Failed { split: cut };
                        notices.push(injected_notice(cut, f, failed));
                    }
                }
                Action::WriteBundle(mut b) => {
                    let startup = match self.startup() {
                        Ok(h) => format!("{h:?}"),
                        Err(e) => format!("failed: {e}"),
                    };
                    let name = b.name();
                    // Only a bundle that will be written needs its build line; the first one computes it here, on the
                    // simulation thread (the hello has computed `build_id` already in a session).
                    if self.io.is_some() {
                        let build = self.build.get_or_insert_with(build_line);
                        b.build = format!("{build}; self-test at start-up {startup}, verdict {self_test:?}");
                        if let Some(f) = injected {
                            let ball = f.ball().map_or_else(|| "none".into(), |b| format!("{:x}", b.to_u128()));
                            b.build.push_str(&format!(
                                " BY INJECTION: {} changed ball {ball} on this machine, a deliberate test fault",
                                f.source()
                            ));
                        }
                    }
                    match self.io.as_ref().map(|io| io.tx.try_send(b)) {
                        Some(Ok(())) => stats.bundles += 1,
                        Some(Err(TrySendError::Full(_) | TrySendError::Disconnected(_))) | None => {
                            stats.bundles_dropped += 1;
                            notices.push(format!("divergence: fault bundle {name} not written"));
                        }
                    }
                }
            }
        }
        out
    }
}

/// The I/O thread for fault bundles (app.md 1): a bounded queue, and a `pe-io` thread writing each under its name.
/// Dropping it closes the queue and waits for what is queued to be written (shutdown, app.md 4).
struct BundleWriter {
    tx: SyncSender<Box<FaultBundle>>,
    thread: Option<JoinHandle<()>>,
}

impl BundleWriter {
    fn start(dir: PathBuf) -> std::io::Result<BundleWriter> {
        let (tx, rx) = sync_channel::<Box<FaultBundle>>(BUNDLE_QUEUE);
        let thread = std::thread::Builder::new().name("pe-io".into()).spawn(move || {
            for b in rx {
                let path = dir.join(b.name());
                let written = b.encode().map_err(|e| format!("{e:?}")).and_then(|bytes| {
                    std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &bytes)).map_err(|e| e.to_string())
                });
                match written {
                    Ok(()) => crate::err!("divergence: fault bundle written to {}", path.display()),
                    Err(e) => crate::err!("divergence: fault bundle {} not written: {e}", path.display()),
                }
            }
        })?;
        Ok(BundleWriter { tx, thread: Some(thread) })
    }
}

impl Drop for BundleWriter {
    fn drop(&mut self) {
        // Closing the queue ends the thread's loop once it has written what is queued.
        let (closed, _) = sync_channel(0);
        drop(std::mem::replace(&mut self.tx, closed));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{Frame, LineId};

    fn split() -> CutRef {
        CutRef { line: LineId::default(), height: 3, frame: Frame(30) }
    }

    /// The same digest passes; a changed digest, or a self-test that could not run, fails (@coordinator-opus).
    #[test]
    fn a_changed_digest_gives_failed() {
        let now = self_test_digest();
        assert_eq!(verdict(&now, &self_test_digest(), split()), SelfTest::Passed { split: split() });
        // pe-physics's own test shows a perturbed scene gives another digest; here, any other digest fails.
        let other = Ok(SelfTestDigest { digest: pe_physics::CheckHash([0x5a; 32]), mode_reset: false });
        assert_ne!(other, now);
        assert_eq!(verdict(&other, &now, split()), SelfTest::Failed { split: split() });
        assert_eq!(verdict(&Err("no".into()), &now, split()), SelfTest::Failed { split: split() });
    }

    /// A bundle goes to the I/O thread and lands in the folder; dropping the writer waits for it.
    #[test]
    fn a_bundle_is_written_by_the_io_thread() {
        // Beside the test binary, in the cargo target folder: no working files on C: (team rule).
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().join(format!("pe-app-bundles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let b = FaultBundle { cut: split(), ..FaultBundle::default() };
        let name = b.name();
        let w = BundleWriter::start(dir.clone()).unwrap();
        w.tx.try_send(Box::new(b.clone())).unwrap();
        drop(w);
        let bytes = std::fs::read(dir.join(&name)).expect("written");
        assert_eq!(FaultBundle::decode(&bytes).unwrap(), b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
