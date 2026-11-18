//! `cargo-crabfarm`: lets `cargo crabfarm <args>` do what `crabfarm cargo
//! <args>` does. Cargo runs it as `cargo-crabfarm crabfarm <args>`; the
//! leading `crabfarm` that cargo inserts is dropped and the rest goes to the
//! same code as `crabfarm cargo`.

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let args = crabfarm::cargo_cmd::args_from_cargo_subcommand(&argv);
    std::process::exit(crabfarm::cargo_cmd::run(&args));
}
