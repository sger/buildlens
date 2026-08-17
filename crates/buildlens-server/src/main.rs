//! The standalone team server.
//!
//! Everything lives in the library beside this file, so the CLI's `dashboard`
//! command and this binary run the same server rather than two
//! implementations that drift.

fn main() -> anyhow::Result<()> {
    buildlens_server::run(buildlens_server::Config::from_env()?)
}
