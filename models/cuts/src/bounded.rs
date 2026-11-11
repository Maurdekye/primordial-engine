//! A depth-bounded exhaustive search: every behaviour of at most `max_depth` steps.
//!
//! Stateright's depth-first search with a depth bound is not exhaustive up to the bound (a state first met at the
//! bound is not expanded, and its visited mark then hides it when a shorter path reaches it: at n = 4, depth 16, it
//! reported 4.2 M states where there are 17.4 M), and its breadth-first search keeps the whole frontier in memory (the
//! n = 4 checks ran out of memory at depth 17). This search keeps, per state fingerprint, the shallowest depth it was
//! reached at, and expands a state again when a shallower path reaches it. It runs breadth-first to a small depth,
//! then depth-first from each state there on several threads sharing the table. Memory is about one fingerprint and
//! one byte per state.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use cuts_model::{Action, Cuts, State};
use stateright::{Expectation, Model, Property};

/// The keys are already hashes.
#[derive(Default)]
pub struct Identity(u64);

impl Hasher for Identity {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, _: &[u8]) {
        unreachable!("only u64 keys")
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = x;
    }
}

type Table = HashMap<u64, u8, BuildHasherDefault<Identity>>;
const SHARDS: usize = 256;

fn fp(s: &State) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

pub struct Bounded {
    pub unique: usize,
    pub visits: usize,
    pub depth: usize,
    /// Each property with a discovery: a counterexample (always) or an example (sometimes), as its action trace.
    pub found: HashMap<&'static str, Vec<Action>>,
}

struct Shared<'a> {
    m: &'a Cuts,
    props: Vec<Property<Cuts>>,
    seen: Vec<Mutex<Table>>,
    found: Mutex<HashMap<&'static str, Vec<Action>>>,
    stop_on: &'a [&'static str],
    stop: AtomicBool,
    visits: AtomicUsize,
    deepest: AtomicUsize,
    max_depth: usize,
}

impl Shared<'_> {
    /// Record a visit at depth d; true if the state is new or now reached at a shallower depth.
    fn fresh(&self, s: &State, d: usize) -> bool {
        self.visits.fetch_add(1, Ordering::Relaxed);
        let f = fp(s);
        let mut t = self.seen[(f >> 56) as usize % SHARDS].lock().unwrap();
        match t.get(&f) {
            Some(&old) if (old as usize) <= d => false,
            _ => {
                t.insert(f, d as u8);
                true
            }
        }
    }

    fn judge(&self, s: &State, trace: &dyn Fn() -> Vec<Action>, d: usize) {
        self.deepest.fetch_max(d, Ordering::Relaxed);
        for p in &self.props {
            let holds = (p.condition)(self.m, s);
            let hit = match p.expectation {
                Expectation::Always => !holds,
                Expectation::Sometimes => holds,
                Expectation::Eventually => false,
            };
            if hit {
                let mut f = self.found.lock().unwrap();
                if !f.contains_key(p.name) {
                    f.insert(p.name, trace());
                    if self.stop_on.contains(&p.name) {
                        self.stop.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    fn successors(&self, s: &State) -> Vec<(Action, State)> {
        let mut acts = Vec::new();
        self.m.actions(s, &mut acts);
        acts.into_iter().filter_map(|a| self.m.next_state(s, a.clone()).map(|n| (a, n))).collect()
    }

    /// Depth-first from a root already recorded at depth `d0`.
    fn dfs(&self, root: State, d0: usize, prefix: Vec<Action>) {
        let mut stack: Vec<(Vec<(Action, State)>, Option<Action>)> = Vec::new();
        if d0 < self.max_depth {
            let mut k = self.successors(&root);
            k.reverse();
            stack.push((k, None));
        }
        while let Some(top) = stack.last_mut() {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            let Some((a, n)) = top.0.pop() else {
                stack.pop();
                continue;
            };
            let d = d0 + stack.len();
            if !self.fresh(&n, d) {
                continue;
            }
            let trace = || {
                let mut t = prefix.clone();
                t.extend(stack.iter().filter_map(|(_, a)| a.clone()));
                t.push(a.clone());
                t
            };
            self.judge(&n, &trace, d);
            if d < self.max_depth {
                let mut k = self.successors(&n);
                k.reverse();
                stack.push((k, Some(a)));
            }
        }
    }
}

pub fn check(m: &Cuts, max_depth: usize, threads: usize, stop_on: &[&'static str], progress: &(dyn Fn(usize, usize) + Sync)) -> Bounded {
    let sh = Shared {
        m,
        props: m.properties(),
        seen: (0..SHARDS).map(|_| Mutex::new(Table::default())).collect(),
        found: Mutex::new(HashMap::new()),
        stop_on,
        stop: AtomicBool::new(false),
        visits: AtomicUsize::new(0),
        deepest: AtomicUsize::new(0),
        max_depth,
    };
    // Breadth-first to a small depth: its states are recorded at their shallowest depth, and those at the last level
    // become the roots of the depth-first searches.
    let init = m.init_states().remove(0);
    sh.fresh(&init, 0);
    sh.judge(&init, &Vec::new, 0);
    let mut level: Vec<(State, Vec<Action>)> = vec![(init, Vec::new())];
    let mut d = 0;
    while d < max_depth && level.len() < threads * 64 && !sh.stop.load(Ordering::Relaxed) {
        let mut next = Vec::new();
        for (s, t) in &level {
            for (a, n) in sh.successors(s) {
                if sh.fresh(&n, d + 1) {
                    let mut t2 = t.clone();
                    t2.push(a);
                    sh.judge(&n, &|| t2.clone(), d + 1);
                    next.push((n, t2));
                }
            }
        }
        level = next;
        d += 1;
    }
    let work = Mutex::new(level);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let item = work.lock().unwrap().pop();
                let Some((s, t)) = item else { break };
                sh.dfs(s, d, t);
            });
        }
        sc.spawn(|| {
            while !work.lock().unwrap().is_empty() || sh.visits.load(Ordering::Relaxed) == 0 {
                std::thread::sleep(std::time::Duration::from_secs(10));
                let u: usize = sh.seen.iter().map(|t| t.lock().unwrap().len()).sum();
                progress(u, sh.visits.load(Ordering::Relaxed));
                if sh.stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        });
    });
    let unique = sh.seen.iter().map(|t| t.lock().unwrap().len()).sum();
    Bounded { unique, visits: sh.visits.load(Ordering::Relaxed), depth: sh.deepest.load(Ordering::Relaxed), found: sh.found.into_inner().unwrap() }
}
