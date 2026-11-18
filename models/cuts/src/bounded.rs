//! A depth-bounded exhaustive search: every behaviour of at most `max_depth` steps.
//!
//! Stateright's depth-first search with a depth bound is not exhaustive up to the bound (a state first met at the
//! bound is not expanded, and its visited mark then hides it when a shorter path reaches it: at n = 4, depth 16, it
//! reported 4.2 M states where there are 17.4 M), and its breadth-first search keeps the whole frontier in memory (the
//! n = 4 checks ran out of memory at depth 17). This search keeps, per state fingerprint, the shallowest depth it was
//! reached at, and expands a state again when a shallower path reaches it. It runs breadth-first to a small depth,
//! then depth-first from each state there on several threads sharing the table. Memory is about one fingerprint and
//! one byte per state.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

fn fp<S: Hash>(s: &S) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

pub struct Bounded<A> {
    pub unique: usize,
    pub visits: usize,
    pub depth: usize,
    /// Each property with a discovery: a counterexample (always) or an example (sometimes), as its action trace.
    pub found: HashMap<&'static str, Vec<A>>,
    /// The search stopped at its time budget before covering every behaviour up to the bound.
    pub unfinished: bool,
}

struct Shared<'a, M: Model> {
    m: &'a M,
    props: Vec<Property<M>>,
    seen: Vec<Mutex<Table>>,
    found: Mutex<HashMap<&'static str, Vec<M::Action>>>,
    stop_on: &'a [&'static str],
    stop: AtomicBool,
    visits: AtomicUsize,
    deepest: AtomicUsize,
    max_depth: usize,
}

impl<M: Model> Shared<'_, M>
where
    M::State: Hash,
    M::Action: Clone,
{
    /// Record a visit at depth d; true if the state is new or now reached at a shallower depth.
    fn fresh(&self, s: &M::State, d: usize) -> bool {
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

    fn judge(&self, s: &M::State, trace: &dyn Fn() -> Vec<M::Action>, d: usize) {
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

    fn successors(&self, s: &M::State) -> Vec<(M::Action, M::State)> {
        let mut acts = Vec::new();
        self.m.actions(s, &mut acts);
        acts.into_iter().filter_map(|a| self.m.next_state(s, a.clone()).map(|n| (a, n))).collect()
    }

    /// Depth-first from a root already recorded at depth `d0`.
    fn dfs(&self, root: M::State, d0: usize, prefix: Vec<M::Action>) {
        let mut stack: Vec<(Vec<(M::Action, M::State)>, Option<M::Action>)> = Vec::new();
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

pub fn check<M>(
    m: &M,
    max_depth: usize,
    threads: usize,
    stop_on: &[&'static str],
    budget: Option<std::time::Duration>,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Bounded<M::Action>
where
    M: Model + Sync,
    M::State: Hash + Send + Sync,
    M::Action: Clone + Send + Sync,
{
    let t0 = std::time::Instant::now();
    let out_of_time = AtomicBool::new(false);
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
    let mut level: Vec<(M::State, Vec<M::Action>)> = vec![(init, Vec::new())];
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
    // The workers still searching: the reporter runs (and enforces the budget) until the last one is done.
    let running = AtomicUsize::new(threads);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                loop {
                    let item = work.lock().unwrap().pop();
                    let Some((s, t)) = item else { break };
                    sh.dfs(s, d, t);
                }
                running.fetch_sub(1, Ordering::Relaxed);
            });
        }
        sc.spawn(|| {
            let mut tick = 0;
            while running.load(Ordering::Relaxed) > 0 {
                std::thread::sleep(std::time::Duration::from_secs(1));
                tick += 1;
                if tick % 10 == 0 {
                    let u: usize = sh.seen.iter().map(|t| t.lock().unwrap().len()).sum();
                    progress(u, sh.visits.load(Ordering::Relaxed));
                }
                if budget.is_some_and(|b| t0.elapsed() >= b) && !sh.stop.load(Ordering::Relaxed) {
                    out_of_time.store(true, Ordering::Relaxed);
                    sh.stop.store(true, Ordering::Relaxed);
                }
            }
        });
    });
    let unique = sh.seen.iter().map(|t| t.lock().unwrap().len()).sum();
    let unfinished = out_of_time.load(Ordering::Relaxed);
    Bounded {
        unique,
        visits: sh.visits.load(Ordering::Relaxed),
        depth: sh.deepest.load(Ordering::Relaxed),
        found: sh.found.into_inner().unwrap(),
        unfinished,
    }
}

/// What a run of seeded random walks covered.
pub struct Walked<A> {
    pub states: usize,
    pub depth: usize,
    /// Walks run (0..walks, all of them unless the time budget cut the run).
    pub walks: u64,
    /// The index of the lowest walk that hit a `stop_on` property: the run stops there.
    pub bit_at: Option<u64>,
    /// Each property with a discovery, from the lowest-index walk that found it.
    pub found: HashMap<&'static str, Vec<A>>,
    pub unfinished: bool,
}

type Found<A> = HashMap<&'static str, (u64, Vec<A>)>;

/// splitmix64: a small fixed generator, so a walk's choices never change with a dependency's version.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Seeded random walks whose outcome doesn't depend on the thread count or on timing. Walk k is seeded from
/// (seed, k) alone, so walks 0..n are the same walks at any thread count. Workers take walk indices in order; when
/// walk k hits a `stop_on` property, no walk above k starts, the walks below k finish, and only walks 0..=k count. So
/// the reported walk, and whether a control bites within n walks, are the same on every run (unless the time budget
/// cuts the run, which is reported as unfinished).
///
/// A walk is stateright's simulation walk: from the initial state, check every property at each state, choose
/// uniformly among the enabled actions, and end with no action left or on a state the walk has already visited.
pub fn walks<M>(
    m: &M,
    walks: u64,
    threads: usize,
    seed: u64,
    stop_on: &[&'static str],
    budget: Option<std::time::Duration>,
    progress: &(dyn Fn(u64, usize) + Sync),
) -> Walked<M::Action>
where
    M: Model + Sync,
    M::State: Hash + Send + Sync,
    M::Action: Clone + Send + Sync,
{
    use std::sync::atomic::{AtomicU32, AtomicU64};
    let t0 = std::time::Instant::now();
    let props = m.properties();
    let next = AtomicU64::new(0);
    // Walks with an index at or above `limit` don't start; a bite at k lowers it to k + 1.
    let limit = AtomicU64::new(walks);
    let out_of_time = AtomicBool::new(false);
    // Per walk: states visited and steps taken, so the totals count exactly the walks that count.
    let per: Vec<(AtomicU32, AtomicU32)> = (0..walks).map(|_| (AtomicU32::new(0), AtomicU32::new(0))).collect();
    let states = AtomicUsize::new(0);
    let done = AtomicU64::new(0);
    // Per property: the lowest walk that found it, and its trace.
    let found: Mutex<Found<M::Action>> = Mutex::new(HashMap::new());
    let running = AtomicUsize::new(threads);
    let walk = |k: u64| {
        let mut rng = mix(seed ^ mix(k));
        let mut s = m.init_states().remove(0);
        let mut path: Vec<M::Action> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut n = 0u32;
        loop {
            if !seen.insert(fp(&s)) {
                break;
            }
            states.fetch_add(1, Ordering::Relaxed);
            n += 1;
            for p in &props {
                let holds = (p.condition)(m, &s);
                let hit = match p.expectation {
                    Expectation::Always => !holds,
                    Expectation::Sometimes => holds,
                    Expectation::Eventually => false,
                };
                if hit {
                    let mut f = found.lock().unwrap();
                    if f.get(p.name).is_none_or(|(j, _)| k < *j) {
                        f.insert(p.name, (k, path.clone()));
                    }
                    if stop_on.contains(&p.name) {
                        limit.fetch_min(k + 1, Ordering::Relaxed);
                    }
                }
            }
            let mut acts = Vec::new();
            m.actions(&s, &mut acts);
            let mut moved = false;
            while !acts.is_empty() {
                rng = mix(rng);
                let a = acts.swap_remove((rng % acts.len() as u64) as usize);
                if let Some(n) = m.next_state(&s, a.clone()) {
                    path.push(a);
                    s = n;
                    moved = true;
                    break;
                }
            }
            if !moved || out_of_time.load(Ordering::Relaxed) {
                break;
            }
        }
        per[k as usize].0.store(n, Ordering::Relaxed);
        per[k as usize].1.store(path.len() as u32, Ordering::Relaxed);
    };
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                loop {
                    if out_of_time.load(Ordering::Relaxed) {
                        break;
                    }
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= limit.load(Ordering::Relaxed) {
                        break;
                    }
                    walk(k);
                    done.fetch_add(1, Ordering::Relaxed);
                }
                running.fetch_sub(1, Ordering::Relaxed);
            });
        }
        sc.spawn(|| {
            let mut tick = 0;
            while running.load(Ordering::Relaxed) > 0 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                tick += 1;
                if tick % 100 == 0 {
                    progress(done.load(Ordering::Relaxed), states.load(Ordering::Relaxed));
                }
                if budget.is_some_and(|b| t0.elapsed() >= b) {
                    out_of_time.store(true, Ordering::Relaxed);
                }
            }
        });
    });
    let lim = limit.load(Ordering::Relaxed);
    let unfinished = out_of_time.load(Ordering::Relaxed);
    let found = found.into_inner().unwrap();
    let bit_at = found.iter().filter(|(p, _)| stop_on.contains(p)).map(|(_, (j, _))| *j).min();
    // Only walks 0..lim count: one that started before a bite lowered the limit is dropped.
    let found = found.into_iter().filter(|(_, (j, _))| *j < lim).map(|(p, (_, t))| (p, t)).collect();
    let counted = &per[..lim as usize];
    Walked {
        states: counted.iter().map(|w| w.0.load(Ordering::Relaxed) as usize).sum(),
        depth: counted.iter().map(|w| w.1.load(Ordering::Relaxed) as usize).max().unwrap_or(0),
        walks: if unfinished { done.load(Ordering::Relaxed).min(lim) } else { lim },
        bit_at,
        found,
        unfinished,
    }
}
