use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            let msg = format!("cannot read current directory: {e}");
            if args.iter().any(|a| a == hub_cli::json::FLAG) {
                let _ = hub_cli::json::emit(
                    &mut std::io::stderr(),
                    hub_cli::json::error_doc(&msg, &[]),
                );
            } else {
                eprintln!("{}: {msg}", hub_cli::PROGRAM);
            }
            return ExitCode::from(hub_cli::json::EXIT_NO_CWD);
        }
    };
    // One command per process: skip freeing the replayed repository (#33).
    hub_cli::set_fast_exit();
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    ExitCode::from(hub_cli::main_with(&args, &cwd, &mut out, &mut err))
}
