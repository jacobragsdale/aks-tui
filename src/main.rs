//! `aks-tui`: a fast terminal browser for AKS clusters — pods, logs, events,
//! configmaps and secrets, one tab per namespace.

fn main() {
    if let Err(error) = aks_tui::run::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
