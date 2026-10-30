//! R16 spike driver. Usage: pe-spike-chunks <precision|cost|far|scale|spatial|regions|players|streaming|all>
//! Build twice: default (box3d f32 positions) and `--features f64` (box3d double-precision).

mod physbench;
mod players;
mod precision;
mod spatialbench;
mod streaming;

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    println!("pe-spike-chunks {arg} (box3d positions: {})", pe_spike_chunks::phys::mode());
    match arg.as_str() {
        "precision" => precision::run(),
        "cost" => physbench::cost(),
        "far" => physbench::far(),
        "scale" => physbench::scale(),
        "spatial" => spatialbench::run(),
        "regions" => players::regions(),
        "players" => players::players(),
        "streaming" => streaming::run(),
        "all" => {
            precision::run();
            physbench::cost();
            physbench::far();
            physbench::scale();
            players::regions();
            players::players();
            streaming::run();
            spatialbench::run();
        }
        other => eprintln!("unknown subcommand {other}"),
    }
}
