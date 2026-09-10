use alloy_eips::BlockId;

/// Constructs JSON-RPC error responses with Base's existing codes and data encoding.
#[derive(Debug)]
pub struct RpcErrorFactory;

impl RpcErrorFactory {
    /// Constructs an invalid params JSON-RPC error.
    pub fn invalid_params(msg: impl Into<String>) -> jsonrpsee_types::error::ErrorObject<'static> {
        Self::with_code_and_data(jsonrpsee_types::error::INVALID_PARAMS_CODE, msg, None)
    }

    /// Constructs an internal JSON-RPC error.
    pub fn internal(msg: impl Into<String>) -> jsonrpsee_types::error::ErrorObject<'static> {
        Self::with_code_and_data(jsonrpsee_types::error::INTERNAL_ERROR_CODE, msg, None)
    }

    /// Constructs an internal JSON-RPC error with code and message
    pub fn with_code(
        code: i32,
        msg: impl Into<String>,
    ) -> jsonrpsee_types::error::ErrorObject<'static> {
        Self::with_code_and_data(code, msg, None)
    }

    /// Constructs a JSON-RPC error, consisting of `code`, `message` and optional `data`.
    pub fn with_code_and_data(
        code: i32,
        msg: impl Into<String>,
        data: Option<&[u8]>,
    ) -> jsonrpsee_types::error::ErrorObject<'static> {
        jsonrpsee_types::error::ErrorObject::owned(
            code,
            msg.into(),
            data.map(|data| {
                jsonrpsee_core::to_json_raw_value(&alloy_primitives::hex::encode_prefixed(data))
                    .expect("serializing String can't fail")
            }),
        )
    }

    /// Formats a [`BlockId`] into an error message.
    pub fn block_id_message(id: BlockId) -> String {
        match id {
            BlockId::Hash(h) => {
                if h.require_canonical == Some(true) {
                    format!("canonical hash {}", h.block_hash)
                } else {
                    format!("hash {}", h.block_hash)
                }
            }
            BlockId::Number(n) => format!("{n}"),
        }
    }
}
