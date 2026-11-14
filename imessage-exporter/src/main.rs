#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]
mod app;
mod db_access_hint;
mod exporters;

pub use exporters::{html::HTML, timeline::Timeline, txt::TXT};

use std::process::ExitCode;

use app::{
    options::{Options, from_command_line},
    runtime::Config,
};

fn main() -> ExitCode {
    // Get args from command line
    let args = from_command_line();
    // Create application options
    let options = Options::from_args(&args);
    // Fork-owned (djf.12 F9): the database the hint names, kept before Config::new takes the options.
    let configured_db = db_access_hint::ConfiguredDb::new(&args, options.as_ref().ok());

    // Create app state and start
    match options {
        Ok(options) => match Config::new(options) {
            Ok(mut app) => {
                // Resolve the filtered contacts, if provided. A filter that matches no chat is an error: exit non-zero, export nothing.
                if let Err(why) = app.resolve_filtered_handles() {
                    eprintln!("Unable to export: {why}");
                    db_access_hint::eprint_db_access_hint(&why, &configured_db);
                    return ExitCode::FAILURE;
                }

                if let Err(why) = app.start() {
                    eprintln!("Unable to export: {why}");
                    db_access_hint::eprint_db_access_hint(&why, &configured_db);
                    return ExitCode::FAILURE;
                }
                ExitCode::SUCCESS
            }
            Err(why) => {
                eprintln!("Invalid configuration: {why}");
                db_access_hint::eprint_db_access_hint(&why, &configured_db);
                ExitCode::FAILURE
            }
        },
        Err(why) => {
            eprintln!("Invalid command line options: {why}");
            db_access_hint::eprint_db_access_hint(&why, &configured_db);
            ExitCode::FAILURE
        }
    }
}
