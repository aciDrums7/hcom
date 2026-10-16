//! Register: `hcom register --tool omp --name <name> [--tag <tag>] --print-env`.
//!
//! Creates (or restores) a named identity for an omp session that hcom did not
//! launch, so a wrapper can start omp with messaging. It is the self-registration
//! step of the SMHL agent-messaging design, not a general launcher.
//!
//! Output with `--print-env` is KEY=VALUE lines only on stdout. Failures exit
//! nonzero with the reason on stderr; the wrapper then starts omp without hcom.

use anyhow::{Result, bail};
use serde_json::json;

use crate::db::HcomDb;
use crate::hooks::omp::PER_RUN;
use crate::hooks::runtime::{self, LaunchCtx};
use crate::identity;
use crate::instance_binding;
use crate::paths;
use crate::router::GlobalFlags;
use crate::tool::Tool;

/// Parsed arguments for `hcom register`.
#[derive(clap::Parser, Debug)]
#[command(name = "register", about = "Register an externally launched session")]
pub struct RegisterArgs {
    /// Tool to register; only `omp` is supported
    #[arg(long)]
    pub tool: String,
    /// Instance name (lowercase letters, digits, underscore)
    #[arg(long)]
    pub name: Option<String>,
    /// Tag for the instance, for example `project:<name>`
    #[arg(long)]
    pub tag: Option<String>,
    /// Print KEY=VALUE lines for a wrapper to export
    #[arg(long)]
    pub print_env: bool,
}

/// Run the register command. `argv` is the full argv including `register`.
pub fn run(argv: &[String], flags: &GlobalFlags) -> Result<i32> {
    use clap::Parser;
    let args = match RegisterArgs::try_parse_from(argv) {
        Ok(a) => a,
        Err(e) => {
            e.print().ok();
            return Ok(if e.use_stderr() { 1 } else { 0 });
        }
    };

    if args.tool != "omp" {
        bail!("register supports only --tool omp, got '{}'", args.tool);
    }
    let Some(name) = args.name.or_else(|| flags.name.clone()) else {
        bail!("register needs --name <name>");
    };
    if !identity::is_valid_base_name(&name) {
        bail!("{}", identity::base_name_error(&name));
    }

    // Publish the plugin first: a failure here must leave no half-registered row.
    let injection = runtime::plan(&PER_RUN, &LaunchCtx::ambient(Tool::Omp, false))?;
    let [flag, plugin] = injection.args.as_slice() else {
        bail!("omp plugin injection did not produce `-e <path>`");
    };
    if flag != "-e" {
        bail!("omp plugin injection did not produce `-e <path>`");
    }

    let db = HcomDb::open()?;
    let hcom_dir = paths::hcom_dir();
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let process_id = uuid::Uuid::new_v4().to_string();

    // A restarted session keeps its name. Its previous process binding is stale,
    // so drop it before binding the new process. The row's saved cursor is not
    // touched: initialize only seeds `last_event_id` when it is 0, so messages
    // queued while the agent was down are delivered on return.
    db.delete_process_bindings_for_instance(&name)?;
    if !instance_binding::initialize_instance_in_position_file(
        &db,
        &name,
        None, // session_id
        None, // parent_session_id
        None, // parent_name
        None, // agent_id
        None, // transcript_path
        Some("omp"),
        false, // background
        args.tag.as_deref(),
        None, // wait_timeout
        None, // subagent_timeout
        None, // hints
        Some(&cwd),
    ) {
        bail!("failed to initialize instance '{name}'");
    }
    db.set_process_binding(&process_id, "", &name)?;

    db.log_event(
        "life",
        &name,
        &json!({
            "action": "registered",
            "tool": "omp",
            "name": name,
        }),
    )
    .ok();

    if args.print_env {
        println!("HCOM_LAUNCHED=1");
        println!("HCOM_PROCESS_ID={process_id}");
        println!("HCOM_INSTANCE_NAME={name}");
        println!("HCOM_DIR={}", hcom_dir.display());
        println!("HCOM_OMP_PLUGIN={plugin}");
    } else {
        println!("registered {name}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rejects_non_omp_tool() {
        let flags = GlobalFlags::default();
        let err = run(
            &argv(&["register", "--tool", "claude", "--name", "luna"]),
            &flags,
        )
        .unwrap_err();
        assert!(err.to_string().contains("only --tool omp"), "{err}");
    }

    #[test]
    fn rejects_invalid_name() {
        let flags = GlobalFlags::default();
        let err = run(
            &argv(&["register", "--tool", "omp", "--name", "My-Name"]),
            &flags,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Invalid instance name"), "{err}");
    }

    #[test]
    #[serial]
    fn restores_saved_cursor_on_reregister() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let flags = GlobalFlags::default();
        let args = argv(&["register", "--tool", "omp", "--name", "luna", "--print-env"]);
        assert_eq!(run(&args, &flags).unwrap(), 0);

        let db = HcomDb::open().unwrap();
        let mut cursor = serde_json::Map::new();
        cursor.insert("last_event_id".into(), json!(3));
        db.update_instance_fields("luna", &cursor).unwrap();

        assert_eq!(run(&args, &flags).unwrap(), 0);
        let row = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(row.last_event_id, 3);
        assert!(db.has_process_binding_for_instance("luna"));
    }
}
