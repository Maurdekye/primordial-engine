//! Sync checks: comparing check hashes at confirmed, fully simulated frames.

use super::*;

impl WorldSim {
    /// Compare check hashes only for confirmed frames this member has finished simulating with the
    /// complete log (viz-1: never while a re-simulation is pending).
    pub(super) fn evaluate_checks(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let fin = self.conf.min(self.frame);
        let frames: Vec<u64> = self.remote_checks.range(..fin).map(|(f, _)| *f).collect();
        for f in frames {
            let Some(&mine) = self.check_hashes.get(&f) else {
                self.remote_checks.remove(&f);
                continue;
            };
            let theirs = self.remote_checks.remove(&f).unwrap_or_default();
            for (m, h) in theirs {
                if !self.evaluated.insert((f, m)) {
                    continue;
                }
                if h == mine {
                    self.stats.checks_ok += 1;
                } else {
                    self.stats.checks_bad += 1;
                    self.events.push(format!(
                        "[w{} m{}] NONDETERMINISM at confirmed frame {f}: mine {mine:016x} member {m} {h:016x}",
                        self.world_id, self.me
                    ));
                }
            }
        }
    }
}
