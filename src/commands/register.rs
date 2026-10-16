//! Register: `hcom register --tool omp --name <name> [--tag <tag>] --print-env`.
//!
//! Creates (or restores) a named identity for an omp session that hcom did not
//! launch, so a wrapper can start omp with messaging. It reuses the bare-start
//! path in `start.rs` with the tool forced to omp.
//!
//! With `--print-env`, stdout is KEY=VALUE lines only. Failures exit nonzero
//! with the reason on stderr; the wrapper then starts omp without hcom.

use anyhow::{Result, bail};

use crate::commands::start;
use crate::db::HcomDb;
use crate::hooks::omp::PER_RUN;
use crate::hooks::runtime::{self, LaunchCtx};
use crate::identity;
use crate::paths;
use crate::router::GlobalFlags;
use crate::shared::context::HcomContext;
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

    // Publish the plugin before touching the DB: a failure leaves no row behind.
    let injection = runtime::plan(&PER_RUN, &LaunchCtx::ambient(Tool::Omp, false))?;
    let [flag, plugin] = injection.args.as_slice() else {
        bail!("omp plugin injection did not produce `-e <path>`");
    };
    if flag != "-e" {
        bail!("omp plugin injection did not produce `-e <path>`");
    }

    let db = HcomDb::open()?;
    let process_id = uuid::Uuid::new_v4().to_string();
    let mut ctx = HcomContext::from_os();
    ctx.process_id = Some(process_id.clone());
    ctx.tool = Tool::Omp;

    if start::register_bare(&db, &ctx, &name, args.tag.as_deref())? != 0 {
        bail!("failed to register '{name}'");
    }

    if args.print_env {
        println!("HCOM_LAUNCHED=1");
        println!("HCOM_PROCESS_ID={process_id}");
        println!("HCOM_INSTANCE_NAME={name}");
        println!("HCOM_DIR={}", paths::hcom_dir().display());
        println!("HCOM_OMP_PLUGIN={plugin}");
    } else {
        println!("registered {name}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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

    #[test]
    #[serial]
    fn reregister_keeps_zero_cursor_so_queued_messages_deliver() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let flags = GlobalFlags::default();
        let args = argv(&["register", "--tool", "omp", "--name", "luna", "--print-env"]);
        assert_eq!(run(&args, &flags).unwrap(), 0);

        let db = HcomDb::open().unwrap();
        let mut cursor = serde_json::Map::new();
        cursor.insert("last_event_id".into(), json!(0));
        db.update_instance_fields("luna", &cursor).unwrap();

        // Message queued while luna was down, after its saved cursor of 0.
        db.log_event(
            "message",
            "luna",
            &json!({"from": "bigboss", "text": "queued", "scope": "mentions", "mentions": ["luna"]}),
        )
        .unwrap();

        assert_eq!(run(&args, &flags).unwrap(), 0);
        let row = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(row.last_event_id, 0, "re-register must not skip queued events");
        let unread = db.get_unread_messages("luna");
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].text, "queued");
    }
}
