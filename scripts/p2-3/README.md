# P2-3 porting scripts

How `crates/pe-session` follows a new round of the spike protocol (`spikes/sim-core/src/session.rs`
on `viewchange`). Run from the repository root.

1. `split_session.py <session.rs> <out dir>`: splits a spike `session.rs` into pe-session's modules
   (entry, membership, log, evict, fetch, check, r14, sim, replay), as S1 did. Then delete the
   `pub use (check|evict|fetch|log)::*;` lines from the output's `mod.rs`.
2. `seam_move.py <dir>` and `seam_rename.py <dir>`: S6b's move of the view change's state into
   `ViewChangeMembership` (`self.vcm.*`). Deterministic and idempotent.
3. Split the previous round (base) and the new one (theirs), apply step 2 to both, then for each
   module `git merge-file -L ours -L base -L theirs crates/pe-session/src/session/<m>.rs base/<m>.rs theirs/<m>.rs`.
   Harness files likewise, with `pe_sim_core` renamed to `pe_session`. Resolve conflicts for the
   S3 types (EntryId, SignedEntry, Sig) and the trait reads in the core modules.
4. Check with `scripts/compare_session_logs.sh --ids <new round>` (with `PE_CLAIM_RULE=spike` for a
   run comparable to the spike) and commit.
