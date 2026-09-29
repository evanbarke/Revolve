//! Browser UI server (built in M3).
use crate::util::Result;
use std::path::Path;

pub fn serve(
    _bind: &str,
    _state_dir: &Path,
    _scenarios_dir: &Path,
    _step_delay_ms: u64,
) -> Result<()> {
    crate::bail!("the browser UI is not built yet in this checkpoint")
}
