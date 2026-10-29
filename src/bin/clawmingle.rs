use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use codex_claw::social::{
    SocialConfig, create_backup, export_backup, list_backups, load_available_models,
    maintain_memories, memory_view, query_backup, resolve_delivery, retry_failed, run, show_status,
    verify_backup,
};

#[tokio::main]
async fn main() {
    if let Err(error) = execute().await {
        // Underlying provider/config errors may include private data. Do not dump them.
        let category = error
            .chain()
            .map(|e| e.to_string())
            .find(|message| {
                message.len() <= 96
                    && message
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            .unwrap_or_else(|| "configuration_or_runtime_failure".into());
        eprintln!("ClawMingle 操作失败（{category}）。未输出可能含敏感信息的底层响应。");
        std::process::exit(1);
    }
}

async fn execute() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() == 1 && args[0] == "commands" {
        println!("{}",serde_json::to_string(&codex_claw::social::command_catalog().iter().map(|(name,desc)|serde_json::json!({"name":name,"desc":desc,"type":"command","only_admin":false})).collect::<Vec<_>>())?);
        return Ok(());
    }
    if args.is_empty() || matches!(args[0].as_str(), "help" | "--help" | "-h") {
        println!(
            "ClawMingle social pilot\nUsage: clawmingle <check|run|status|models|capability-catalog|stickers> --config /absolute/path/social.toml\n       clawmingle integration-check --plugin <plugin-id> [--plugin <plugin-id> ...] --config /absolute/path/social.toml\n       clawmingle retry <group-index> --config /absolute/path/social.toml\n       clawmingle resolve <group-index> <request-number> <bubble-index> <sent|not-sent> --config /absolute/path/social.toml\n       clawmingle <backup|backup-list> --config /absolute/path/social.toml\n       clawmingle backup-verify <backup-name> --config /absolute/path/social.toml\n       clawmingle backup-query <backup-name> <group|private> <conversation-index> <after-cursor> --config /absolute/path/social.toml\n       clawmingle backup-export <backup-name> /absolute/new-directory --config /absolute/path/social.toml\nNo arguments start no services. Resolve and manual backup require a stopped bot. Backup export never overwrites active data."
        );
        return Ok(());
    }
    let position = args
        .iter()
        .position(|arg| arg == "--config")
        .ok_or_else(|| anyhow::anyhow!("explicit_config_required"))?;
    let path = PathBuf::from(
        args.get(position + 1)
            .ok_or_else(|| anyhow::anyhow!("config_path_required"))?,
    );
    ensure!(position + 2 == args.len(), "unexpected_arguments");
    let config = SocialConfig::load(&path).context("social_config_load_failed")?;
    match args[0].as_str() {
        "documents-admin" => {
            ensure!(position == 1, "unexpected_arguments");
            codex_claw::social::documents_admin_operation(&config).await
        }
        "stickers" => {
            ensure!(position == 1, "unexpected_arguments");
            codex_claw::social::sticker_operation(&config)
        }
        "memory-operation" => {
            ensure!(position == 1, "unexpected_arguments");
            codex_claw::social::memory_operation(&config)
        }
        "behavior" => {
            ensure!(position == 1, "unexpected_arguments");
            println!("{}", codex_claw::social::behavior_settings(&config));
            Ok(())
        }
        "check" => {
            codex_claw::social::check_group_profiles(&config)?;
            ensure!(position == 1, "unexpected_arguments");
            println!("社交配置格式检查通过。启动时自动准备通用工具环境；独立认证仍需配置。");
            Ok(())
        }
        "codex-binary" => {
            ensure!(position == 1, "unexpected_arguments");
            let binary = codex_claw::social::resolve_codex_binary(&config.codex.binary)?;
            println!("{}", serde_json::json!({"binary": binary}));
            Ok(())
        }
        "models" => {
            ensure!(position == 1, "unexpected_arguments");
            let catalog = load_available_models(&config).await?;
            println!("{}", serde_json::to_string(&catalog)?);
            Ok(())
        }
        "capability-catalog" => {
            ensure!(position == 1, "unexpected_arguments");
            println!(
                "{}",
                serde_json::to_string(&codex_claw::social::capability_catalog(&config).await?)?
            );
            Ok(())
        }
        "integration-check" => {
            ensure!(
                position >= 3 && position % 2 == 1,
                "integration_check_arguments_required"
            );
            let mut plugins = Vec::new();
            for pair in args[1..position].as_chunks::<2>().0 {
                ensure!(
                    pair[0] == "--plugin",
                    "integration_check_arguments_required"
                );
                plugins.push(pair[1].clone());
            }
            let result = codex_claw::social::check_integrations(&config, &plugins).await?;
            println!("{}", serde_json::to_string(&result)?);
            Ok(())
        }
        "run" => {
            ensure!(position == 1, "unexpected_arguments");
            run(config).await
        }
        "status" => {
            ensure!(position == 1, "unexpected_arguments");
            show_status(&config)
        }
        "backup" => {
            ensure!(position == 1, "unexpected_arguments");
            println!("{}", serde_json::to_string(&create_backup(&config)?)?);
            Ok(())
        }
        "backup-list" => {
            ensure!(position == 1, "unexpected_arguments");
            println!("{}", serde_json::to_string(&list_backups(&config)?)?);
            Ok(())
        }
        "backup-verify" => {
            ensure!(position == 2, "backup_name_required");
            println!(
                "{}",
                serde_json::to_string(&verify_backup(&config, &args[1])?)?
            );
            Ok(())
        }
        "backup-query" => {
            ensure!(position == 5, "backup_query_arguments_required");
            println!(
                "{}",
                serde_json::to_string(&query_backup(
                    &config,
                    &args[1],
                    &args[2],
                    args[3].parse()?,
                    args[4].parse()?
                )?)?
            );
            Ok(())
        }
        "backup-export" => {
            ensure!(position == 3, "backup_export_arguments_required");
            println!(
                "{}",
                serde_json::to_string(&export_backup(
                    &config,
                    &args[1],
                    PathBuf::from(&args[2]).as_path()
                )?)?
            );
            Ok(())
        }
        "memory-view" => {
            ensure!(position == 2, "group_index_required");
            memory_view(&config, args[1].parse()?)
        }
        "memory-maintain" => {
            ensure!(position == 1 || position == 2, "unexpected_arguments");
            maintain_memories(
                &config,
                if position == 2 {
                    Some(args[1].parse()?)
                } else {
                    None
                },
            )
        }
        "resolve" => {
            ensure!(
                position == 5 && matches!(args[4].as_str(), "sent" | "not-sent"),
                "invalid_resolution_arguments"
            );
            resolve_delivery(
                &config,
                args[1].parse()?,
                args[2].parse()?,
                args[3].parse()?,
                args[4] == "sent",
            )
        }
        "retry" => {
            ensure!(position == 2, "group_index_required");
            retry_failed(&config, args[1].parse()?)
        }
        _ => anyhow::bail!("unknown_command"),
    }
}
