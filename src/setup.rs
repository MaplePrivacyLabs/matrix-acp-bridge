//! Offline configuration wizard. Never reads credentials or contacts a service.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use crate::config::{Config, ConversationMode, HarnessConfig, RoomPolicy};

fn ask(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    default: &str,
) -> Result<String> {
    if default.is_empty() {
        write!(output, "{label}: ")?;
    } else {
        write!(output, "{label} [{default}]: ")?;
    }
    output.flush()?;
    let mut line = String::new();
    ensure!(
        input.read_line(&mut line)? != 0,
        "input ended; no configuration was written"
    );
    let value = line.trim();
    Ok(if value.is_empty() {
        default.into()
    } else {
        value.into()
    })
}

fn ids(text: &str) -> BTreeSet<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn path(text: String) -> Result<PathBuf> {
    let value = PathBuf::from(text);
    ensure!(
        value.is_absolute(),
        "use an absolute path (expand ~ yourself)"
    );
    Ok(value)
}

/// Exclusive creation prevents an accidental reset of an existing worker's policy.
pub fn write_config(file: &Path, config: &Config) -> Result<()> {
    config.validate()?;
    let content = toml::to_string_pretty(config)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut target = options
        .open(file)
        .context("cannot create config (existing files are never overwritten)")?;
    target.write_all(content.as_bytes())?;
    target.sync_all()?;
    Ok(())
}

fn gather(
    input: &mut impl BufRead,
    output: &mut impl Write,
    base: &Path,
    service: bool,
) -> Result<Config> {
    writeln!(
        output,
        "Create one bot profile. IDs and paths only; do not enter passwords or tokens."
    )?;
    let bot_user_id = ask(
        input,
        output,
        "Bot Matrix ID (e.g. @my-agent:example.org)",
        "",
    )?;
    let homeserver = ask(
        input,
        output,
        "Homeserver HTTPS URL (from your Matrix client settings)",
        "",
    )?;
    let room_id = ask(
        input,
        output,
        "Encrypted room ID (starts with !, from Room settings > Advanced)",
        "",
    )?;
    let operators = ids(&ask(
        input,
        output,
        "Operator Matrix IDs (comma-separated)",
        "",
    )?);
    let mut audience = BTreeSet::new();
    audience.extend(operators.iter().cloned());
    audience.insert(bot_user_id.clone());
    let state_dir = if service {
        PathBuf::from("/var/lib/matrix-acp-bridge/state")
    } else {
        base.join("state")
    };
    let harness = if service {
        let mode = ask(
            input,
            output,
            "ACP mode (optional; leave blank for agent defaults)",
            "",
        )?;
        HarnessConfig {
            program: "/usr/bin/sudo".into(),
            args: vec![
                "-n".into(),
                "-u".into(),
                "matrix-acp-agent".into(),
                "--".into(),
                "/usr/local/libexec/matrix-acp-agent".into(),
            ],
            workspace: "/var/lib/matrix-acp-agent/workspace".into(),
            env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            mode: (!mode.is_empty()).then_some(mode),
            steering: Default::default(),
        }
    } else {
        let program = path(ask(input, output, "ACP executable (absolute path)", "")?)?;
        let args: Vec<String> = serde_json::from_str(&ask(
            input,
            output,
            "ACP arguments (JSON array, e.g. [\"acp\"])",
            "[]",
        )?)
        .context("arguments must be a JSON array of strings, not a shell command")?;
        let workspace = path(ask(
            input,
            output,
            "Agent workspace (absolute path; must already exist)",
            base.to_str().context("non-UTF8 path")?,
        )?)?;
        let mode = ask(
            input,
            output,
            "ACP mode (optional; leave blank for agent defaults)",
            "",
        )?;
        let home = path(ask(
            input,
            output,
            "Agent HOME (absolute path, where you authenticated the adapter)",
            "",
        )?)?;
        let search_path = ask(
            input,
            output,
            "Agent PATH (include its language runtime if needed)",
            "/usr/local/bin:/usr/bin:/bin",
        )?;
        HarnessConfig {
            program,
            args,
            workspace,
            mode: (!mode.is_empty()).then_some(mode),
            steering: Default::default(),
            env: BTreeMap::from([
                ("HOME".into(), home.to_string_lossy().into_owned()),
                ("PATH".into(), search_path),
            ]),
        }
    };
    let config = Config {
        bot_user_id,
        homeserver,
        state_dir,
        harness,
        rooms: vec![RoomPolicy {
            room_id,
            operators,
            operator_trust: crate::config::OperatorTrust::Account,
            audience,
            audience_policy: crate::config::AudiencePolicy::RoomMembership,
            conversation: ConversationMode::Thread,
            tool_approval: Default::default(),
        }],
        approval_ttl_seconds: 300,
        max_concurrent_runs: 1,
        tools_socket: None,
    };
    config.validate()?;
    Ok(config)
}

pub fn interactive(file: &Path, service: bool) -> Result<()> {
    ensure!(
        !file.exists(),
        "config already exists; edit it directly instead of reinitializing"
    );
    let absolute = std::path::absolute(file)?;
    let base = absolute
        .parent()
        .context("config has no parent directory")?;
    let config = gather(&mut io::stdin().lock(), &mut io::stdout(), base, service)?;
    write_config(&absolute, &config)?;
    println!(
        "Created {}. Review its operators, audience, and harness mode.",
        absolute.display()
    );
    println!("Next: doctor, enroll, then run (use --config for this file).");
    if service {
        println!(
            "Before using the service profile: chown root:matrix-acp-bridge CONFIG; chmod 0640 CONFIG."
        );
    } else {
        println!(
            "Direct mode shares your OS identity with the agent. Use a dedicated worker, or see docs/LINUX-SERVICE.md for separate service users."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wizard_round_trip_preserves_argv_and_audience_and_writes_privately() {
        let temp = tempfile::tempdir().unwrap();
        let answers = "@bot:example.org\nhttps://matrix.example.org\n!room:example.org\n@alice:example.org\n/usr/bin/agent\n[\"acp\",\"path with spaces\"]\n\nread-only\n/agent/home\n\n";
        let config = gather(&mut answers.as_bytes(), &mut Vec::new(), temp.path(), false).unwrap();
        let file = temp.path().join("config.toml");
        write_config(&file, &config).unwrap();
        let parsed = Config::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(parsed.harness.args, ["acp", "path with spaces"]);
        assert_eq!(parsed.rooms[0].audience.len(), 2);
        assert_eq!(
            parsed.rooms[0].audience_policy,
            crate::config::AudiencePolicy::RoomMembership
        );
        assert_eq!(
            parsed.rooms[0].operator_trust,
            crate::config::OperatorTrust::Account
        );
        assert_eq!(parsed.rooms[0].operators.len(), 1);
        assert_eq!(parsed.state_dir, temp.path().join("state"));
        assert!(write_config(&file, &config).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn interrupted_or_invalid_setup_never_produces_a_profile() {
        assert!(
            gather(
                &mut "@bot:example.org\n".as_bytes(),
                &mut Vec::new(),
                Path::new("/tmp"),
                false
            )
            .is_err()
        );
        let answers =
            "@bot:example.org\nhttps://example.org\n!room:example.org\n@bot:example.org\n\nagent\n";
        assert!(
            gather(
                &mut answers.as_bytes(),
                &mut Vec::new(),
                Path::new("/tmp"),
                true
            )
            .is_err()
        );
    }
}
