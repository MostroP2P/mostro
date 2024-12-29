use clap::{Parser, Subcommand};
use std::env;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
struct XtaskArgs {
    #[command(subcommand)]
    command: Commands,
    #[arg(short, long)]
    verbose: bool,
}

// Commands enum
#[derive(Subcommand)]
enum Commands {
    // Build all the project
    #[command(verbatim_doc_comment)]
    BuildAll,
    // Build
    #[command(verbatim_doc_comment)]
    Build,
    // Build db
    #[command(verbatim_doc_comment)]
    BuildDb,
    #[command(verbatim_doc_comment)]
    Clean,
}

// Get the project root
fn project_root() -> PathBuf {
    Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap()
        .to_path_buf()
}

// Get the cargo command
pub fn cargo() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

// Clean the project
fn clean_project(verbose: bool) {
    tracing::info!("Cleaning project....");
    let mut cargo_cmd = Command::new(cargo());
    let mut cmd = cargo_cmd.current_dir(project_root()).args(["clean"]);
    if !verbose {
        cmd = cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let status = cmd.status().expect("Running clean failed");
    if !status.success() {
        tracing::error!("Failed to clean project");
        exit(-1);
    }
}

// Build the mostro db
fn build_mostro_db(verbose: bool) {
    tracing::info!("Building mostro db....");
    env::set_var("DATABASE_URL", "sqlite://mostro.db");
    tracing::info!("DATABASE_URL: {}", env::var("DATABASE_URL").unwrap());

    tracing::info!("Removing old database files");
    let mut cargo_cmd = Command::new("rm");
    let mut cmd =
        cargo_cmd
            .current_dir(project_root())
            .args(["-rf", "mostro.db*", "sqlx-data.json"]);
    if !verbose {
        cmd = cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let status = cmd.status().expect("Running rm failed");
    if !status.success() {
        tracing::error!("Failed to remove old database files");
        exit(-1);
    }
    tracing::info!("Creating new database");
    let mut cargo_cmd = Command::new("sqlx");
    let mut cmd = cargo_cmd
        .current_dir(project_root())
        .args(["database", "create"]);
    if !verbose {
        cmd = cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let status = cmd.status().expect("Running sqlx database create failed");
    if !status.success() {
        tracing::error!("Failed to create database");
        exit(-1);
    }
    tracing::info!("Running migrations");
    let mut cargo_cmd = Command::new("sqlx");
    let mut cmd = cargo_cmd
        .current_dir(project_root())
        .args(["migrate", "run"]);
    if !verbose {
        cmd = cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let status = cmd.status().expect("Running sqlx migrate run failed");
    if !status.success() {
        tracing::error!("Failed to run migrations");
        exit(-1);
    }
    tracing::info!("Running sqlx prepare");
    let mut cargo_cmd = Command::new(cargo());
    let mut cmd = cargo_cmd
        .current_dir(project_root())
        .args(["sqlx", "prepare", "--merged"]);
    if !verbose {
        cmd = cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let status = cmd.status().expect("Running sqlx prepare failed");
    if !status.success() {
        tracing::error!("Failed to prepare database");
        exit(-1);
    }
    tracing::info!("Running sqlx prepare check");
    let mut cargo_cmd = Command::new(cargo());
    let mut cmd = cargo_cmd
        .current_dir(project_root())
        .args(["sqlx", "prepare", "--check", "--merged"]);
    if !verbose {
        cmd = cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let status = cmd.status().expect("Running sqlx prepare check failed");
    if !status.success() {
        tracing::error!("Failed to prepare database");
        exit(-1);
    }
    tracing::info!("Mostro db created successfully");
}

fn build_all(verbose: bool) {
    tracing::info!("Building all....");
    let mut cargo_cmd = Command::new(cargo());
    let mut cmd = cargo_cmd
        .current_dir(project_root())
        .args(["build", "--release"]);
    if !verbose {
        cmd = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .arg("--quiet");
    }
    let status = cmd.status().expect("Running Cargo failed");
    if !status.success() {
        tracing::error!("All Mostro crates build failed");
        exit(-1);
    }
    tracing::info!("All Mostro crates build successfully!");
}

// Main function
fn main() {
    // Adding some info tracing just for logging activity
    env::set_var("RUST_LOG", "info");

    // Tracing using RUST_LOG
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let args = XtaskArgs::parse();

    match args.command {
        // Build all the project
        Commands::BuildAll => {
            clean_project(args.verbose);
            build_mostro_db(args.verbose);
            build_all(args.verbose);
        }
        // Build the project
        Commands::Build => {
            build_all(args.verbose);
        }
        // Build the mostro db
        Commands::BuildDb => {
            build_mostro_db(args.verbose);
        }
        // Clean the project
        Commands::Clean => {
            clean_project(args.verbose);
        }
    }
}
