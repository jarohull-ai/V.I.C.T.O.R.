use victor_daemon::{parse_args, Daemon};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), victor_daemon::DaemonError> {
    Daemon::prepare(parse_args(std::env::args())?)
        .await?
        .run()
        .await
}
