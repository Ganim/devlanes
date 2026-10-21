//! Remote entry points that run in a terminal or over SSH: `devlanes tunnel --stdio`
//! (started on the box by the client's SSH transport) and `devlanes askpass` (run by ssh).

use devlanes_cli::Action;
use devlanes_proto::RpcError;
use serde_json::json;

/// Routes the remote-only actions.
pub async fn run(action: Action) -> Result<u8, RpcError> {
    match action {
        Action::Tunnel | Action::Askpass(_) => Err(RpcError::new(
            "not_supported_yet",
            json!({"milestone": "M14"}),
            2,
        )),
        _ => Err(RpcError::new(
            "usage",
            json!({"message": "unexpected action"}),
            2,
        )),
    }
}
