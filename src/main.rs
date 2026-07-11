mod app;
mod capture;
mod cli;
mod config;
mod terminal;

fn main() {
    if let Err(error) = app::run() {
        eprintln!("lumi: {error:#}");
        std::process::exit(1);
    }
}
