mod backup;
mod catalog;
mod cli;
mod config;
#[cfg(test)]
mod e2e_tests;
mod format;
mod restore;
mod s3;
mod schedule;
mod snapshot;
mod store;
mod util;

fn main() {
    std::process::exit(cli::main());
}
