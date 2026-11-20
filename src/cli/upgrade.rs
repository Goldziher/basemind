//! `basemind upgrade --plan`: the dry run of a release-minor rebuild ([`crate::upgrade_plan`]).

use std::io::Write;

use anyhow::{Context, Result};
use clap::Args;

use crate::cli::exit::CliExit;
use crate::cli::render::render_json;

/// Arguments of `basemind upgrade`.
#[derive(Args, Debug)]
pub struct UpgradeArgs {
    /// Print, per repository group and workspace, what the next scan rebuilds after a release-minor
    /// bump, the bytes that frees and needs, and a scan estimate. Reads only; changes nothing.
    #[arg(long)]
    pub plan: bool,
}

/// Run `basemind upgrade`. The rebuild itself happens at each workspace's next scan, so the command
/// only plans: without `--plan` it is a usage error naming the flag.
pub fn run(args: &UpgradeArgs, json: bool, out: &mut impl Write) -> Result<()> {
    if !args.plan {
        return Err(CliExit::usage(
            "`basemind upgrade` needs `--plan`: the rebuild runs at each workspace's next scan \
             (`basemind scan --root <worktree>` rebuilds one), and `--plan` shows what it will do",
        ));
    }
    let data = crate::data_root::DataRoot::current();
    let headroom = crate::config::engine::with(|config| config.min_free_bytes);
    let plan = crate::upgrade_plan::plan(&data, headroom).context("plan the upgrade")?;
    if json {
        let value = serde_json::to_value(&plan).context("serialize the upgrade plan")?;
        render_json(&value, out)
    } else {
        crate::upgrade_plan::render_human(&plan, out).context("write the upgrade plan")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_without_plan_is_a_usage_error_naming_the_flag() {
        let error = run(&UpgradeArgs { plan: false }, false, &mut Vec::new()).expect_err("refused");

        assert_eq!(crate::cli::exit::exit_code_for(&error), crate::cli::exit::USAGE);
        assert!(error.to_string().contains("`--plan`"), "{error}");
    }
}
