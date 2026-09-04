//! drowse: CLI client. Each subcommand is exactly one RPC plus a formatter;
//! JSON is the default output when stdout is not a tty.

fn main() {
    eprintln!("drowse: not implemented yet");
    std::process::exit(drowse_api::ErrorCode::Usage.exit_code());
}
