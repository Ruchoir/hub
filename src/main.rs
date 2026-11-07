use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    hub::run().await
}
