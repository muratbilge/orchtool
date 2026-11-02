#[tokio::main(flavor = "current_thread")]
async fn main() {
    eprintln!(
        "orchd: phase 0 skeleton, protocol {}; PTY service is not implemented",
        orch_proto::VERSION
    );
    std::process::exit(2);
}
