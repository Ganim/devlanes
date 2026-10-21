//! Remote is mirroring only: a client never runs daemon requests on the box, so
//! every request stream is refused.

use async_trait::async_trait;
use devlanes_proto::RpcError;
use devlanes_transport::{RpcHandler, RpcInput, RpcReplyStream};
use serde_json::{Value, json};

use crate::rpc_error;

/// [`RpcHandler`] that refuses every request without reading it.
pub(crate) struct Refuse;

#[async_trait]
impl RpcHandler for Refuse {
    /// Always `not_supported`.
    async fn call(
        &self,
        _request: Value,
        _input: RpcInput,
    ) -> Result<RpcReplyStream, devlanes_transport::RpcError> {
        Err(rpc_error(RpcError::new(
            "not_supported",
            json!({
                "feature": "remote commands",
                "hint": "devlanes connect mirrors a box; run devlanes on the box itself",
            }),
            1,
        )))
    }
}
