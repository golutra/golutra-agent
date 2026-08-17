use std::{env, ffi::OsString, path::PathBuf, process::Command};

use golutra_release::ReleaseStore;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if matches!(
        env::args_os().nth(1),
        Some(argument) if argument == "--version" || argument == "-V"
    ) {
        println!("golutra-launcher {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let release_home = env::var_os("GOLUTRA_RELEASE_HOME")
        .map(PathBuf::from)
        .ok_or("GOLUTRA_RELEASE_HOME is required")?;
    let mut args = env::args_os().skip(1);
    let binary = args
        .next()
        .or_else(|| env::var_os("GOLUTRA_RELEASE_BINARY"))
        .ok_or("usage: golutra-launcher <binary> [args...]")?;
    let binary_name = binary.to_str().ok_or("release binary name must be UTF-8")?;
    let store = ReleaseStore::new(release_home)?;
    let stable = store
        .pointer("stable")?
        .ok_or("no stable release is selected")?;
    let path = store.binary_path(&stable.release_id, binary_name)?;
    launch(path, args.collect(), stable.release_id)
}

#[cfg(unix)]
fn launch(
    path: PathBuf,
    args: Vec<OsString>,
    release_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;

    let error = Command::new(path)
        .args(args)
        .env("GOLUTRA_RELEASE_ID", release_id)
        .exec();
    Err(Box::new(error))
}

#[cfg(not(unix))]
fn launch(
    path: PathBuf,
    args: Vec<OsString>,
    release_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new(path)
        .args(args)
        .env("GOLUTRA_RELEASE_ID", release_id)
        .status()?;
    std::process::exit(status.code().unwrap_or(1));
}
