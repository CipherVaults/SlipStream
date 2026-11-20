use crate::{
    framing::{read_frame, write_frame},
    pipe::{error_response, PipeClient},
    token::SessionToken,
};
use slipstream_protocol::NativeBridgeRequest;
use std::{
    io::{self, Read, Write},
    path::Path,
};

pub fn forward(
    input: &mut impl Read,
    output: &mut impl Write,
    pipe_name: &str,
    token_path: &Path,
    client_id: &str,
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut client = None;
    loop {
        let request = match read_frame::<NativeBridgeRequest>(input) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(_) => {
                return write_frame(
                    output,
                    &error_response("invalid_frame", "Invalid browser native messaging frame"),
                )
            }
        };
        let response = match request {
            NativeBridgeRequest::Authenticate { .. } => error_response(
                "invalid_request",
                "The native host authenticates on behalf of the browser",
            ),
            request => runtime.block_on(async {
                let connection = match client.take() {
                    Some(connection) => Ok(connection),
                    None => match SessionToken::read(token_path) {
                        Ok(token) => PipeClient::connect(pipe_name, &token, client_id).await,
                        Err(error) => Err(error),
                    },
                };
                if let Ok(connection) = connection {
                    if let Ok((connection, response)) = connection.request(&request).await {
                        client = Some(connection);
                        return response;
                    }
                }
                // A command might already have reached the daemon. Retry the connection
                // on the next browser request without replaying a possibly mutating command.
                error_response(
                    "pipe_unavailable",
                    "The SlipStream desktop connection is unavailable",
                )
            }),
        };
        write_frame(output, &response)?;
    }
}
