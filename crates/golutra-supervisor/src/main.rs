use std::{fs, path::PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use golutra_protocol::TaskTracePage;
use golutra_supervisor::{
    CandidateProposal, CandidateRequest, DeploymentObservation, EvolutionEpochBudget,
    EvolutionSupervisor, ExternalCommandProducer, InternalCommandProducer, RuntimeEvaluationSuite,
};

const MAX_CONTROL_INPUT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "golutra-supervisor")]
#[command(about = "Golutra governed evolution and release control plane")]
struct Cli {
    #[arg(long, env = "GOLUTRA_SUPERVISOR_HOME")]
    root: PathBuf,
    #[arg(long, env = "GOLUTRA_RELEASE_HOME")]
    releases: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Status,
    VerifyLog,
    Bootstrap {
        #[arg(value_name = "SOURCE_ROOT")]
        source: PathBuf,
    },
    ObserveTrace {
        #[arg(value_name = "JSON_FILE")]
        file: PathBuf,
        #[arg(long, default_value = "cli")]
        independent_group: String,
    },
    StartEpoch {
        opportunity_id: String,
        #[arg(long, default_value_t = 3)]
        max_candidates: u32,
        #[arg(long, default_value_t = 2)]
        max_holdout_queries: u32,
    },
    PrepareWorktree {
        epoch_id: String,
        candidate_id: String,
    },
    RegisterCandidate {
        #[arg(value_name = "JSON_FILE")]
        file: PathBuf,
    },
    Produce {
        #[arg(value_name = "REQUEST_JSON")]
        request: PathBuf,
        #[arg(long, value_name = "PROGRAM")]
        program: PathBuf,
        #[arg(long, value_enum, default_value_t = ProducerArg::External)]
        kind: ProducerArg,
        #[arg(long = "arg")]
        args: Vec<String>,
    },
    Evaluate {
        #[arg(value_name = "JSON_FILE")]
        file: PathBuf,
    },
    Check {
        candidate_id: String,
    },
    Build {
        candidate_id: String,
    },
    Preview {
        candidate_id: String,
    },
    Canary {
        candidate_id: String,
    },
    CanaryObservation {
        #[arg(value_name = "JSON_FILE")]
        file: PathBuf,
    },
    Promote {
        candidate_id: String,
        #[arg(long)]
        reason: String,
    },
    Rollback {
        candidate_id: String,
        #[arg(long)]
        reason: String,
    },
    EnforceDeadlines,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProducerArg {
    Internal,
    External,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let supervisor = EvolutionSupervisor::new(cli.root, cli.releases)
        .map_err(|error| miette::miette!("{error}"))?;
    let output = match cli.command {
        Command::Status => serde_json::to_value(supervisor.store().snapshot()?)?,
        Command::VerifyLog => serde_json::to_value(supervisor.store().verify_control_log()?)?,
        Command::Bootstrap { source } => {
            serde_json::to_value(supervisor.bootstrap_stable_release(source).await?)?
        }
        Command::ObserveTrace {
            file,
            independent_group,
        } => {
            let page: TaskTracePage = read_json(&file)?;
            serde_json::to_value(supervisor.observe_trace(&page, &independent_group)?)?
        }
        Command::StartEpoch {
            opportunity_id,
            max_candidates,
            max_holdout_queries,
        } => {
            let budget = EvolutionEpochBudget {
                max_candidates,
                max_holdout_queries,
                ..EvolutionEpochBudget::default()
            };
            serde_json::to_value(supervisor.start_epoch(&opportunity_id, budget)?)?
        }
        Command::PrepareWorktree {
            epoch_id,
            candidate_id,
        } => {
            serde_json::to_value(supervisor.prepare_candidate_worktree(&epoch_id, &candidate_id)?)?
        }
        Command::RegisterCandidate { file } => {
            let proposal: CandidateProposal = read_json(&file)?;
            serde_json::to_value(supervisor.register_candidate(proposal)?)?
        }
        Command::Produce {
            request,
            program,
            kind,
            args,
        } => {
            let request: CandidateRequest = read_json(&request)?;
            match kind {
                ProducerArg::Internal => {
                    let producer = InternalCommandProducer::new(program)?.with_args(args);
                    serde_json::to_value(
                        supervisor.produce_and_register(&producer, request).await?,
                    )?
                }
                ProducerArg::External => {
                    let producer = ExternalCommandProducer::new(program)?.with_args(args);
                    serde_json::to_value(
                        supervisor.produce_and_register(&producer, request).await?,
                    )?
                }
            }
        }
        Command::Evaluate { file } => {
            let suite: RuntimeEvaluationSuite = read_json(&file)?;
            serde_json::to_value(supervisor.evaluate_suite(suite).await?)?
        }
        Command::Check { candidate_id } => {
            serde_json::to_value(supervisor.run_trusted_build(&candidate_id).await?)?
        }
        Command::Build { candidate_id } => {
            serde_json::to_value(supervisor.build_verified_release(&candidate_id)?)?
        }
        Command::Preview { candidate_id } => {
            serde_json::to_value(supervisor.preview(&candidate_id)?)?
        }
        Command::Canary { candidate_id } => {
            serde_json::to_value(supervisor.start_canary(&candidate_id)?)?
        }
        Command::CanaryObservation { file } => {
            let observation: DeploymentObservation = read_json(&file)?;
            serde_json::to_value(supervisor.record_canary_observation(observation)?)?
        }
        Command::Promote {
            candidate_id,
            reason,
        } => serde_json::to_value(supervisor.promote(&candidate_id, &reason)?)?,
        Command::Rollback {
            candidate_id,
            reason,
        } => serde_json::to_value(supervisor.rollback(&candidate_id, &reason)?)?,
        Command::EnforceDeadlines => serde_json::to_value(supervisor.enforce_deadlines()?)?,
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &PathBuf) -> miette::Result<T> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| miette::miette!("failed to inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_CONTROL_INPUT_BYTES
    {
        return Err(miette::miette!(
            "control input violates its file boundary: {}",
            path.display()
        ));
    }
    let content = fs::read_to_string(path)
        .map_err(|error| miette::miette!("failed to read {}: {error}", path.display()))?;
    serde_json::from_str(&content)
        .map_err(|error| miette::miette!("invalid JSON in {}: {error}", path.display()))
}
