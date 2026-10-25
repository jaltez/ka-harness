//! The landlock self-exec trampoline, usable by any embedder binary:
//! `<exe> ka-sandbox-exec <policy-json> -- <argv…>` applies the
//! landlock ruleset to itself, then execs the real argv (restrictions
//! survive execve). Fails closed: any landlock error refuses to run.

/// The hidden subcommand name [`wrap_command`] re-execs this binary
/// with when landlock is the enforcement backend.
pub const TRAMPOLINE_ARG: &str = "ka-sandbox-exec";

/// Apply the landlock policy named by `policy_json`, then exec `argv`.
/// Returns only on failure (exec never returns on success). The policy
/// JSON is `{"writable":[…], "net":bool, "env":[…]}` — the net/env
/// fields carry sandbox-expansion grants.
#[cfg(target_os = "linux")]
pub fn exec_trampoline(
    policy_json: &str,
    argv: &[String],
) -> Result<std::convert::Infallible, String> {
    use std::os::unix::process::CommandExt;

    let parsed: serde_json::Value =
        serde_json::from_str(policy_json).map_err(|e| format!("sandbox policy: {e}"))?;
    let writable: Vec<std::path::PathBuf> = parsed
        .get("writable")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(std::path::PathBuf::from))
                .collect()
        })
        .unwrap_or_default();
    let allow_net = parsed.get("net").and_then(|v| v.as_bool()).unwrap_or(false);
    let env_names: Vec<String> = parsed
        .get("env")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "sandbox-exec: no command after --".to_string())?;
    apply_landlock(&writable, allow_net)?;
    // parity with bwrap --clearenv: the sandboxed command must not see
    // the parent's environment (provider API keys live there). PATH is
    // re-set to the bare minimum so plain commands still resolve; env
    // grants re-set their variables from this process's environment
    // (the trampoline still sees the parent's values).
    let passed: Vec<(String, String)> = env_names
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
        .collect();
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    cmd.env_clear();
    cmd.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    for (name, value) in passed {
        cmd.env(name, value);
    }
    let err = cmd.exec();
    // exec() only returns on failure
    Err(format!("sandbox-exec: {program}: {err}"))
}

/// Landlock policy: read everything beneath `/`; write only beneath the
/// allowlist; network (bind/connect) denied when the kernel ABI ≥ 4 and
/// the policy does not grant it (older kernels degrade silently —
/// filesystem enforcement stays). Mirrors what bwrap's `--unshare-all`
/// gives the fs tier.
#[cfg(target_os = "linux")]
fn apply_landlock(writable: &[std::path::PathBuf], allow_net: bool) -> Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, Compatible, LandlockStatus, Ruleset, RulesetAttr,
        RulesetCreatedAttr, path_beneath_rules,
    };

    // newest ABI the crate knows: IoctlDev (V5) and later rights get
    // denied by default too; BestEffort at restrict_self degrades
    // gracefully on older kernels (they simply don't have the rights)
    let abi = ABI::V9;
    let ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| format!("sandbox: ruleset: {e}"))?;
    // handled-but-ungranted network access = denied on ABI ≥ 4;
    // BestEffort makes these no-ops where the kernel lacks it. An
    // expansion grant skips the handling entirely: unhandled = allowed.
    let ruleset = if allow_net {
        ruleset
    } else {
        ruleset
            .handle_access(AccessNet::BindTcp)
            .map_err(|e| format!("sandbox: ruleset (net bind): {e}"))?
            .handle_access(AccessNet::ConnectTcp)
            .map_err(|e| format!("sandbox: ruleset (net connect): {e}"))?
    };
    let created = ruleset
        .create()
        .map_err(|e| format!("sandbox: ruleset create: {e}"))?
        .add_rules(path_beneath_rules(["/"], AccessFs::from_read(abi)))
        .map_err(|e| format!("sandbox: read rules: {e}"))?
        .add_rules(path_beneath_rules(writable.iter(), AccessFs::from_all(abi)))
        .map_err(|e| format!("sandbox: write rules: {e}"))?;
    let status = created
        .set_compatibility(landlock::CompatLevel::BestEffort)
        .restrict_self()
        .map_err(|e| format!("sandbox: landlock enforce: {e}"))?;
    match status.landlock {
        LandlockStatus::Available { .. } => Ok(()),
        LandlockStatus::NotEnabled | LandlockStatus::NotImplemented => Err(
            "sandbox: kernel landlock not enabled on this host — refusing to run unsandboxed"
                .to_string(),
        ),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn trampoline_arg_matches_wrap_command() {
        // wrap_command's argv[1] and the subcommand name must stay in
        // lockstep — a drift breaks every landlock host silently. Only
        // checkable where landlock is the enforcement backend.
        if crate::detect_tool() != Some(crate::Tool::Landlock) {
            return;
        }
        let p = crate::Policy::fs(vec!["/tmp".into()]);
        let argv = crate::wrap_command(&p, "true", std::path::Path::new("/tmp")).expect("wrap ok");
        assert_eq!(argv[1], TRAMPOLINE_ARG, "{argv:?}");
    }

    #[test]
    fn exec_trampoline_refuses_bad_policy_and_empty_argv() {
        let err = exec_trampoline("not json", &["true".to_string()]).unwrap_err();
        assert!(err.contains("sandbox policy"), "{err}");
        let policy = serde_json::json!({"writable": [], "net": false, "env": []}).to_string();
        let err = exec_trampoline(&policy, &[]).unwrap_err();
        assert!(err.contains("no command"), "{err}");
    }
}
