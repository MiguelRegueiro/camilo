mod app;
mod capture;
mod cli;
mod config;
mod terminal;

fn main() {
    if let Err(error) = app::run() {
        eprintln!("camilo: {error:#}");
        std::process::exit(1);
    }
}
