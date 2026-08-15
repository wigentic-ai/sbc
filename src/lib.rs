mod cli;
mod clipboard;
mod command;
mod config;
mod session;
mod target;
mod transfer;

use std::error::Error;

pub(crate) type BoxError = Box<dyn Error + Send + Sync>;
pub(crate) type Result<T> = std::result::Result<T, BoxError>;

pub fn run() -> Result<()> {
    match cli::parse()? {
        cli::Invocation::Connect(args) => {
            let config = config::Config::load(args.config.as_deref())?;
            let target = target::Target::resolve(&args, &config)?;
            let command = command::SessionCommand::for_target(&target, &args.command)?;
            session::run(command, target, !args.no_clipboard)
        }
        cli::Invocation::Config(args) => config::run(args),
    }
}
