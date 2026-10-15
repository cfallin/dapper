//! The full-screen terminal UI (placeholder; see below).

use crate::core::Dapper;
use anyhow::Result;
use std::process::ChildStdout;

pub fn run(_d: &mut Dapper, _stdout: Option<ChildStdout>, _script: Option<&str>) -> Result<()> {
    anyhow::bail!("not yet implemented")
}
