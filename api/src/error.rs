use serde::Serialize;
use serde_json;
use std::sync::PoisonError;
use thiserror::Error;

#[derive(Debug, Serialize)]
pub struct JsonError {
    code: JsonErrCode,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JsonErrCode {
    Parse = -32700,
    InvalidReq = -32600,
    MethodNotFound = -32601,
    InvalidParam = -32602,
    Internal = -32603,
    NoStream = -32000,
    NoControl = -32001,
    PlayerPoison = -32002,
}

impl JsonError {
    pub fn parse(data: Option<String>) -> Self {
        Self {
            code: JsonErrCode::Parse,
            message: "Parse error".to_string(),
            data,
        }
    }

    pub fn invalid_request(data: Option<String>) -> Self {
        Self {
            code: JsonErrCode::InvalidReq,
            message: "Invalid Request".to_string(),
            data,
        }
    }

    pub fn method_not_found(data: Option<String>) -> Self {
        Self {
            code: JsonErrCode::MethodNotFound,
            message: "Method not found".to_string(),
            data,
        }
    }

    pub fn invalid_param(data: Option<String>) -> Self {
        Self {
            code: JsonErrCode::InvalidParam,
            message: "Invalid params".to_string(),
            data,
        }
    }

    pub fn internal(data: Option<String>) -> Self {
        Self {
            code: JsonErrCode::Internal,
            message: "Internal jsonrpc error".to_string(),
            data,
        }
    }

    pub fn no_control(data: Option<String>) -> Self {
        Self {
            code: JsonErrCode::NoControl,
            message: "No player to control".to_string(),
            data,
        }
    }
}

impl<T> From<tokio::sync::broadcast::error::SendError<T>> for JsonError {
    fn from(value: tokio::sync::broadcast::error::SendError<T>) -> Self {
        Self::internal(Some(value.to_string()))
    }
}

impl From<serde_json::Error> for JsonError {
    fn from(value: serde_json::Error) -> Self {
        Self::parse(Some(format!("{value:#?}")))
    }
}
