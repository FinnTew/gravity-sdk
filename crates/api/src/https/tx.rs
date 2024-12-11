use std::sync::Arc;

use api_types::{ExecTxn, ExecutionApiV2};
use aptos_crypto::HashValue;
use aptos_logger::info;
use axum::{http::StatusCode, response::Json as JsonResponse};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct TxRequest {
    tx: Vec<u8>,
    //    Public key and signature to authenticate
    //    authenticator: (),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TxResponse {
    pub tx: Vec<u8>,
    // tx status
}

// example:
// curl -X POST -H "Content-Type:application/json" -d '{"tx": [1, 2, 3, 4]}' https://127.0.0.1:1998/tx/submit_tx
pub async fn submit_tx(
    request: TxRequest,
    execution_api: Option<Arc<dyn ExecutionApiV2>>,
) -> Result<JsonResponse<()>, StatusCode> {
    if let Some(execution_api) = execution_api {
        let res = execution_api.add_txn(ExecTxn::RawTxn(request.tx)).await;
        if let Err(e) = res {
            info!("submit tx error {:?}", e);
            return Err(StatusCode::from_u16(1).unwrap());
        }
    }
    Ok(JsonResponse(()))
}

// example:
// curl https://127.0.0.1:1998/tx/get_tx_by_hash/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
pub async fn get_tx_by_hash(
    request: HashValue,
    execution_api: Option<Arc<dyn ExecutionApiV2>>,
) -> Result<JsonResponse<TxResponse>, StatusCode> {
    info!("get transaction by hash {}", request);
    Ok(JsonResponse(TxResponse { tx: vec![] }))
}