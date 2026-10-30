mod cli;
mod daemon;
mod events;
mod fingerprint;
mod graph;
mod proto;
mod python;
mod report;
mod root;
mod run;
mod schedule;
mod select;
mod store;
mod threads;
mod zygote;

fn main() {
    std::process::exit(cli::main(std::env::args().skip(1).collect()));
}
