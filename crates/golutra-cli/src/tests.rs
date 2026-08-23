use super::*;

#[test]
fn separate_cli_commands_use_the_same_controller_identity() {
    let session_id = SessionId::new();
    let takeover = command(
        session_id,
        SessionCommandKind::Takeover,
        serde_json::json!({}),
    );
    let abort = command(session_id, SessionCommandKind::Abort, serde_json::json!({}));

    assert_eq!(takeover.actor, abort.actor);
    assert_eq!(takeover.actor.id, CLI_ACTOR_ID);
}

#[test]
fn chat_uses_the_model_owned_full_defaults() {
    assert_eq!(
        chat_prompt_payload("inspect the workspace".to_owned()),
        serde_json::json!({
            "prompt": "inspect the workspace",
            "execution_mode": "open",
            "tool_profile": "full",
        })
    );
}

#[test]
fn run_directory_implies_ephemeral_exec_and_keeps_legacy_alias() {
    let cli = Cli::try_parse_from([
        "golutra",
        "exec",
        "--run-dir",
        "/tmp/golutra-run",
        "inspect the workspace",
    ])
    .expect("persisted run exec");
    assert!(matches!(
        cli.command,
        Command::Exec(ExecArgs {
            ephemeral: false,
            run_dir: Some(path),
            ..
        }) if path == std::path::Path::new("/tmp/golutra-run")
    ));

    let legacy = Cli::try_parse_from([
        "golutra",
        "exec",
        "--ephemeral-state-dir",
        "/tmp/golutra-run",
        "inspect the workspace",
    ])
    .expect("legacy run-dir alias");
    assert!(matches!(
        legacy.command,
        Command::Exec(ExecArgs {
            run_dir: Some(path),
            ..
        }) if path == std::path::Path::new("/tmp/golutra-run")
    ));
}

#[test]
fn persisted_run_accepts_thread_resume_for_external_correction() {
    let cli = Cli::try_parse_from([
        "golutra",
        "--run-bundle",
        "/tmp/golutra-run",
        "exec",
        "resume",
        "00000000-0000-0000-0000-000000000001",
        "fix the failed assertion",
    ])
    .expect("persisted run resume");
    assert!(matches!(
        cli.command,
        Command::Exec(ExecArgs {
            command: Some(ExecCommand::Resume { .. }),
            ..
        })
    ));
}

#[test]
fn exec_can_disable_project_verifier_discovery() {
    let cli = Cli::try_parse_from([
        "golutra",
        "exec",
        "--no-project-verifier-discovery",
        "inspect the workspace",
    ])
    .expect("exec verifier discovery opt-out");

    assert!(matches!(
        cli.command,
        Command::Exec(ExecArgs {
            no_project_verifier_discovery: true,
            ..
        })
    ));
}

#[test]
fn exec_accepts_a_positive_per_turn_elapsed_budget() {
    let cli = Cli::try_parse_from([
        "golutra",
        "exec",
        "--max-elapsed-ms",
        "345000",
        "inspect the workspace",
    ])
    .expect("exec elapsed budget");

    assert!(matches!(
        cli.command,
        Command::Exec(ExecArgs {
            max_elapsed_ms: Some(value),
            ..
        }) if value.get() == 345_000
    ));
    assert!(
        Cli::try_parse_from([
            "golutra",
            "exec",
            "--max-elapsed-ms",
            "0",
            "inspect the workspace",
        ])
        .is_err()
    );
}

#[test]
fn yolo_parses_for_embedded_daemon_connect_and_resume_exec() {
    for arguments in [
        vec!["golutra", "exec", "--yolo", "modify files"],
        vec!["golutra", "--daemon", "exec", "--yolo", "modify files"],
        vec![
            "golutra",
            "--connect",
            "http://127.0.0.1:47831",
            "exec",
            "--yolo",
            "modify files",
        ],
        vec![
            "golutra",
            "exec",
            "resume",
            "00000000-0000-0000-0000-000000000001",
            "--yolo",
            "modify files",
        ],
    ] {
        let cli = Cli::try_parse_from(&arguments).expect("yolo exec");
        assert!(
            matches!(cli.command, Command::Exec(ExecArgs { yolo: true, .. })),
            "{arguments:?}"
        );
    }
}

#[test]
fn approval_mode_accepts_codex_style_aliases() {
    for (value, expected) in [
        ("on-request", ExecApprovalModeArg::Prompt),
        ("granular", ExecApprovalModeArg::Prompt),
        ("never", ExecApprovalModeArg::Deny),
    ] {
        let cli = Cli::try_parse_from(["golutra", "exec", "--approval-mode", value, "inspect"])
            .expect("approval mode alias");
        assert!(matches!(
            cli.command,
            Command::Exec(ExecArgs { approval_mode, .. }) if approval_mode == expected
        ));
    }
}

#[test]
fn repeated_external_evaluation_uses_its_original_trace_binding() {
    let mut value = serde_json::json!({
        "base_trace_digest": "auto",
        "runtime_identity": "auto",
    });

    apply_external_evaluation_binding_defaults(
        &mut value,
        "sha256:current-overlay-trace",
        "build:current",
        Some(("sha256:original-source-trace", "build:original")),
    );

    assert_eq!(value["base_trace_digest"], "sha256:original-source-trace");
    assert_eq!(value["runtime_identity"], "build:original");

    let mut first_ingest = serde_json::json!({});
    apply_external_evaluation_binding_defaults(
        &mut first_ingest,
        "sha256:current-source-trace",
        "build:current",
        None,
    );
    assert_eq!(
        first_ingest["base_trace_digest"],
        "sha256:current-source-trace"
    );
    assert_eq!(first_ingest["runtime_identity"], "build:current");
}

#[test]
fn evolution_commands_parse_governed_budget_and_skill_review() {
    let plan = Cli::try_parse_from([
        "golutra",
        "evolution",
        "plan",
        "expand provider robustness",
        "--max-selected-tasks",
        "2",
    ])
    .expect("evolution plan");
    assert!(matches!(
        plan.command,
        Command::Evolution {
            command: EvolutionCommand::Plan {
                max_selected_tasks: 2,
                ..
            }
        }
    ));

    let review = Cli::try_parse_from([
        "golutra",
        "evolution",
        "skill",
        "review",
        "skill-runtime-tests",
        "--decision",
        "approve",
        "--reason",
        "regression passed",
        "--regression-ref",
        "regression-1",
    ])
    .expect("skill review");
    assert!(matches!(
        review.command,
        Command::Evolution {
            command: EvolutionCommand::Skill {
                command: EvolutionSkillCommand::Review {
                    decision: ReviewDecisionArg::Approve,
                    ..
                }
            }
        }
    ));
}

#[test]
fn plugin_commands_parse_explicit_review_and_enable_steps() {
    let review = Cli::try_parse_from(["golutra", "plugin", "review", "fixture", "revision-1"])
        .expect("plugin review");
    assert!(matches!(
        review.command,
        Command::Plugin {
            command: PluginCommand::Review {
                plugin_id,
                revision_id,
            }
        } if plugin_id == "fixture" && revision_id == "revision-1"
    ));

    let enable = Cli::try_parse_from(["golutra", "plugin", "enable", "fixture", "revision-1"])
        .expect("plugin enable");
    assert!(matches!(
        enable.command,
        Command::Plugin {
            command: PluginCommand::Enable { .. }
        }
    ));
}

#[test]
fn export_command_requires_destination_and_accepts_anchor_range() {
    let cli = Cli::try_parse_from([
        "golutra",
        "export",
        "/tmp/golutra-export",
        "--thread-id",
        "01900000-0000-7000-8000-000000000001",
        "--range",
        "+50",
    ])
    .expect("export args");
    assert!(matches!(
        cli.command,
        Command::Export {
            range,
            destination,
            thread_id: Some(_),
        } if range == "+50" && destination == std::path::Path::new("/tmp/golutra-export")
    ));
}

#[test]
fn provider_set_key_accepts_disk_or_environment_reference() {
    let disk = Cli::try_parse_from([
        "golutra",
        "provider",
        "set-key",
        "--profile",
        "custom",
        "--api-key",
        "test-key",
        "--store",
        "disk",
    ])
    .expect("disk args");
    let Command::Provider { command } = &disk.command else {
        panic!("expected provider command");
    };
    assert!(matches!(
        command.as_ref(),
        ProviderCommand::SetKey {
            api_key: Some(_),
            env_key: None,
            store: CredentialStoreArg::Disk,
            ..
        }
    ));

    let environment = Cli::try_parse_from([
        "golutra",
        "provider",
        "set-key",
        "--profile",
        "custom",
        "--env-key",
        "CUSTOM_API_KEY",
    ])
    .expect("environment args");
    let Command::Provider { command } = &environment.command else {
        panic!("expected provider command");
    };
    assert!(matches!(
        command.as_ref(),
        ProviderCommand::SetKey {
            api_key: None,
            env_key: Some(_),
            ..
        }
    ));
}

#[test]
fn provider_oauth_login_requires_an_explicit_descriptor_file() {
    let cli = Cli::try_parse_from([
        "golutra",
        "provider",
        "oauth-login",
        "--descriptor",
        "provider-oauth.json",
        "--flow",
        "device",
        "--base-url",
        "https://api.example.com/v1",
        "--model",
        "example-model",
    ])
    .expect("OAuth args");

    let Command::Provider { command } = &cli.command else {
        panic!("expected provider command");
    };
    assert!(matches!(
        command.as_ref(),
        ProviderCommand::OAuthLogin {
            flow: Some(OAuthFlowArg::Device),
            store: CredentialStoreArg::Disk,
            ..
        }
    ));
}

#[test]
fn provider_auth_methods_and_builtin_oauth_login_are_parsed() {
    let methods = Cli::try_parse_from([
        "golutra",
        "provider",
        "auth-methods",
        "--provider",
        "openai-chatgpt",
    ])
    .expect("auth methods args");
    let Command::Provider { command } = &methods.command else {
        panic!("expected provider command");
    };
    assert!(matches!(
        command.as_ref(),
        ProviderCommand::AuthMethods {
            provider: Some(provider)
        } if provider == "openai-chatgpt"
    ));

    let login = Cli::try_parse_from([
        "golutra",
        "provider",
        "oauth-login",
        "--provider",
        "openai-chatgpt",
        "--method",
        "browser",
    ])
    .expect("builtin OAuth args");
    let Command::Provider { command } = &login.command else {
        panic!("expected provider command");
    };
    assert!(matches!(
        command.as_ref(),
        ProviderCommand::OAuthLogin {
            provider: Some(provider),
            method: Some(method),
            descriptor: None,
            ..
        } if provider == "openai-chatgpt" && method == "browser"
    ));
}

#[test]
fn builtin_openai_oauth_resolves_registered_responses_adapter() {
    let login = resolve_oauth_login(
        None,
        Some("openai-chatgpt"),
        Some("browser"),
        None,
        None,
        None,
        None,
        None,
    )
    .expect("resolve builtin OpenAI OAuth");

    assert_eq!(login.flow, OAuthFlow::BrowserPkce);
    assert_eq!(login.protocol, ProviderProtocol::OpenAiResponses);
    assert_eq!(login.base_url, "https://chatgpt.com/backend-api/codex");
    assert_eq!(login.model, "gpt-5.5");
    assert_eq!(
        login.descriptor.browser_redirect_uri.as_deref(),
        Some("http://localhost:1455/auth/callback")
    );
    let headless = resolve_oauth_login(
        None,
        Some("openai-chatgpt"),
        Some("headless"),
        None,
        None,
        None,
        None,
        None,
    )
    .expect("resolve builtin OpenAI headless OAuth");
    assert_eq!(headless.flow, OAuthFlow::OpenAiDeviceAuth);
    assert_eq!(headless.protocol, ProviderProtocol::OpenAiResponses);
    assert!(headless.descriptor.openai_device_authorization.is_some());

    assert!(
        resolve_oauth_login(
            None,
            Some("openai-chatgpt"),
            Some("browser"),
            None,
            None,
            Some("openai-compatible"),
            None,
            None,
        )
        .is_err()
    );
    assert!(
        resolve_oauth_login(
            None,
            Some("openai-chatgpt"),
            Some("browser"),
            None,
            None,
            None,
            Some("https://example.com/v1".to_owned()),
            None,
        )
        .is_err()
    );
}

#[test]
fn evaluation_artifact_base_accepts_a_bare_relative_filename() {
    let base = evaluation_artifact_base_path(std::path::Path::new("evaluation.json"), None)
        .expect("relative evaluation base");

    assert_eq!(
        base,
        std::env::current_dir()
            .expect("current directory")
            .canonicalize()
            .expect("canonical current directory")
    );
}

#[test]
fn evaluation_ingest_accepts_an_explicit_artifact_base() {
    let cli = Cli::try_parse_from([
        "golutra",
        "eval",
        "ingest",
        "--artifact-base",
        "/tmp/terminal-bench-trial",
        "/tmp/golutra-run/terminal-bench-evaluation.json",
    ])
    .expect("evaluation ingest args");

    assert!(matches!(
        cli.command,
        Command::Eval {
            command: EvalCommand::Ingest {
                file,
                artifact_base: Some(artifact_base),
            }
        } if file == std::path::Path::new("/tmp/golutra-run/terminal-bench-evaluation.json")
            && artifact_base == std::path::Path::new("/tmp/terminal-bench-trial")
    ));
}

#[test]
fn evaluation_artifact_base_prefers_the_explicit_directory() {
    let directory = tempfile::tempdir().expect("artifact base");
    let base = evaluation_artifact_base_path(
        std::path::Path::new("/unrelated/evaluation.json"),
        Some(directory.path()),
    )
    .expect("explicit artifact base");

    assert_eq!(
        base,
        directory
            .path()
            .canonicalize()
            .expect("canonical artifact base")
    );
}

#[test]
fn project_service_cli_keeps_persistent_backends_outside_runtime_flags() {
    let tmux = Cli::try_parse_from([
        "golutra",
        "--cwd",
        "/tmp/project",
        "service",
        "start",
        "web",
        "tmux",
        "npm",
        "run",
        "dev",
        "--",
        "--host",
        "0.0.0.0",
    ])
    .expect("tmux service args");
    assert!(matches!(
        tmux.command,
        Command::Service {
            command: ServiceCommand::Start {
                name,
                backend: ServiceStartBackend::Tmux { command },
            }
        } if name == "web" && command.first().is_some_and(|value| value == "npm")
    ));

    let compose = Cli::try_parse_from([
        "golutra",
        "service",
        "start",
        "stack",
        "docker-compose",
        "--file",
        "deploy/compose.yaml",
        "--service",
        "api",
    ])
    .expect("compose service args");
    assert!(matches!(
        compose.command,
        Command::Service {
            command: ServiceCommand::Start {
                backend: ServiceStartBackend::DockerCompose { services, .. },
                ..
            }
        } if services == ["api"]
    ));
}
