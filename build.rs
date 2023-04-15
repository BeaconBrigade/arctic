use std::io::Result;

fn main() -> Result<()> {
    prost_build::compile_protos(
        &[
            "src/proto/google/protobuf/descriptor.proto",
            "src/proto/types.proto",
            "src/proto/pftp_error.proto",
            "src/proto/pftp_request.proto",
            "src/proto/pftp_response.proto",
            "src/proto/exercise_samples.proto",
            "src/proto/pftp_notification.proto",
            "src/proto/exercise_rr_samples.proto",
        ],
        &["src/proto/"],
    )?;
    Ok(())
}
