//! Ball-pit video matrix: renders what the engaging player and an observer actually display
//! during and after the hand sweep of spike 5d, across latency x bandwidth x protocol
//! configuration. See docs/ballpit-video-matrix.md.
//!
//! usage: pe-ballpit-viz [--out DIR] [--only SUBSTRING] [--no-video] [--no-cells] [--no-grid] [--software]

mod analyze;
mod overlay;
mod record;
mod render;
mod video;

use std::fmt::Write as _;

use analyze::*;
use record::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| args.iter().position(|x| x == k).and_then(|i| args.get(i + 1)).cloned();
    let has = |k: &str| args.iter().any(|x| x == k);
    let out = get("--out").unwrap_or_else(|| "docs/media/ballpit-matrix".into());
    let only = get("--only");
    let cells: Vec<Cell> = matrix().into_iter().filter(|c| only.as_ref().is_none_or(|o| c.id.contains(o.as_str()))).collect();
    std::fs::create_dir_all(format!("{out}/cells")).unwrap();
    std::fs::create_dir_all(format!("{out}/metrics")).unwrap();

    let mut recs: Vec<(Recording, Analysis)> = Vec::new();
    let mut notes = String::new();
    for c in &cells {
        let t0 = std::time::Instant::now();
        let r = record(c);
        let a = analyze(&r);
        println!("== {} ({} frames, recorded in {:.1} s)", c.id, r.eng.len(), t0.elapsed().as_secs_f64());
        writeln!(notes, "== {}  [{} | {}]", c.id, c.mode_label(), c.net_label()).unwrap();
        for n in &r.notes {
            println!("   {n}");
            writeln!(notes, "   {n}").unwrap();
        }
        write_csv(&format!("{out}/metrics/{}.csv", c.id), &r, &a);
        recs.push((r, a));
    }
    let table = summary_table(&recs);
    println!("{table}");
    std::fs::write(format!("{out}/summary.md"), &table).unwrap();
    std::fs::write(format!("{out}/verification.txt"), &notes).unwrap();
    // Snap-threshold calibration: the engager never rolls back in these cells.
    for (r, a) in &recs {
        if r.cell.mode != Mode::Naive && a.summary.eng_rollbacks_s == 0.0 {
            let (n, f): (u64, Vec<u64>) = (a.summary.eng_snaps, a.eng.iter().zip(&r.eng).filter(|(d, _)| d.snaps > 0).map(|(_, s)| s.k).collect());
            println!("calibration {}: engager (no rollbacks) snap ball-frames {n} at ticks {:?}", r.cell.id, f);
        }
    }
    if has("--no-video") {
        return;
    }
    let mut rd = render::Renderer::new(has("--software"));
    let ai = &rd.adapter_info;
    let gpu = format!("wgpu adapter: {} ({:?}, {:?})", ai.name, ai.backend, ai.device_type);
    println!("{gpu}");
    std::fs::write(format!("{out}/adapter.txt"), format!("{gpu}\n")).unwrap();
    let text = overlay::Text::load();
    let colors = video::ball_colors();
    if !has("--no-cells") {
        for (r, a) in &recs {
            let t0 = std::time::Instant::now();
            let path = format!("{out}/cells/{}.mp4", r.cell.id);
            video::cell_video(&mut rd, &text, &colors, r, a, &path);
            println!("wrote {path} in {:.1} s", t0.elapsed().as_secs_f64());
        }
    }
    if !has("--no-grid") {
        let t0 = std::time::Instant::now();
        let path = format!("{out}/ballpit-matrix-grid.mp4");
        video::grid_video(&mut rd, &text, &colors, &recs, &path);
        println!("wrote {path} in {:.1} s", t0.elapsed().as_secs_f64());
    }
}

fn write_csv(path: &str, r: &Recording, a: &Analysis) {
    let mut s = String::from(
        "tick,t_s,peer,sim_frame,tx_Bps,rx_Bps,rollbacks_per_s,resim_ms_per_frame,queue_ms,queue_drops,lost,snapshot_bytes,keyframes,conf_lag,stalled,restamps,snaps,snaps_per_s,div_balls_gt_1cm,div_max_m,div_mean_m,hand_div_m\n",
    );
    for (peer, shots, ds) in [("engager", &r.eng, &a.eng), ("observer", &r.obs, &a.obs)] {
        for (sh, d) in shots.iter().zip(ds.iter()) {
            let m = sh.m;
            writeln!(
                s,
                "{},{:.4},{peer},{},{:.0},{:.0},{:.1},{:.3},{:.1},{},{},{},{},{},{},{},{},{:.0},{},{:.4},{:.5},{:.4}",
                sh.k, (sh.k * DT_US + EPOCH_US) as f64 / 1e6, sh.sim_frame, m.tx_bps, m.rx_bps, m.rollbacks_s, m.resim_ms, m.queue_ms, m.cap_drops, m.lost,
                m.snapshot_bytes, m.snapshots, m.conf_lag, m.stalled as u8, m.restamps, d.snaps, d.snaps_s, d.div_count, d.div_max_m, d.div_mean_m, d.hand_div_m
            )
            .unwrap();
        }
    }
    std::fs::write(path, s).unwrap();
}

fn summary_table(recs: &[(Recording, Analysis)]) -> String {
    let mut t = String::from(
        "| Cell | Engager hand lag behind controller | Observer hand lag | Engager rollbacks/s (sweep) | Observer rollbacks/s, re-sim ms/frame (sweep) | Engager tx (sweep) | Observer tx (sweep) | Max queue, queue drops | Peak snapshot memory | Balls >1 cm off at the observer: sweep mean / peak | Snapped ball-frames: engager / observer | Converged after gesture end | Final state |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for (r, a) in recs {
        let s = &a.summary;
        let fin = if r.notes[0].contains("IDENTICAL") { "identical".to_string() } else { format!("DIFFERENT ({} balls off, max {:.2} m)", s.final_div_count, s.final_div_max_m) };
        let conv = match s.converge_s {
            Some(x) if x == 0.0 => "by gesture end".to_string(),
            Some(x) => format!("{x:.2} s"),
            None => "never (within 4.3 s)".into(),
        };
        let lagms = |f: f64| format!("{f:.1} frames ({:.0} ms)", f * 1000.0 / HZ as f64);
        writeln!(
            t,
            "| `{}` | {} | {} | {:.1} | {:.1}, {:.2} | {:.1} kB/s | {:.1} kB/s | {:.0} ms, {} | {:.1} MB | {:.1} / {} | {} / {} | {} | {} |",
            r.cell.id,
            lagms(s.eng_hand_lag_frames),
            lagms(s.obs_hand_lag_frames),
            s.eng_rollbacks_s,
            s.sweep_rollbacks_s,
            s.sweep_resim_ms,
            s.sweep_tx_eng_bps / 1e3,
            s.sweep_tx_obs_bps / 1e3,
            s.max_queue_ms,
            s.cap_drops,
            s.max_snapshot_bytes as f64 / 1e6,
            s.sweep_mean_div_count,
            s.max_div_count,
            s.eng_snaps,
            s.obs_snaps,
            conv,
            fin
        )
        .unwrap();
    }
    t
}
