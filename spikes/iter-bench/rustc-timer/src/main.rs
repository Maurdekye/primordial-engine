//! RUSTC_WRAPPER that times each rustc invocation and logs it (crate, ms, cwd, argv) as one JSON
//! line to $RUSTC_TIMER_LOG. `rustc-timer --noop` exits at once (process-spawn cost probe).

use std::io::Write;
use std::time::Instant;

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--noop") {
        return;
    }
    let t = Instant::now();
    let status = std::process::Command::new(&args[0]).args(&args[1..]).status().expect("spawn rustc");
    let ms = t.elapsed().as_secs_f64() * 1e3;
    if let Ok(log) = std::env::var("RUSTC_TIMER_LOG") {
        let name = args.windows(2).find(|w| w[0] == "--crate-name").map_or("", |w| w[1].as_str());
        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        let argv: Vec<String> = args.iter().map(|a| esc(a)).collect();
        let line = format!(
            "{{\"crate\":{},\"ms\":{ms:.1},\"status\":{},\"cwd\":{},\"argv\":[{}]}}\n",
            esc(name),
            status.code().unwrap_or(-1),
            esc(&cwd),
            argv.join(",")
        );
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
            let _ = f.write_all(line.as_bytes());
        }
    }
    std::process::exit(status.code().unwrap_or(1));
}
