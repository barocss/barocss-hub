use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hub: cannot read current directory: {e}");
            return ExitCode::from(2);
        }
    };
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    match hub_cli::run(&args, &cwd, &mut out, &mut err) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hub: {e}");
            ExitCode::FAILURE
        }
    }
}
