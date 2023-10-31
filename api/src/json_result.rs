use std::fmt::Display;

use serde::Serialize;
use serde_json;
use thiserror::Error;

pub type JsonResult = Result<JsonResponse, JsonError>;

#[derive(Debug, Serialize)]
pub struct JsonResponse {
    id: i64,
    jsonrpc: f32,
    result: serde_json::Value,
}

#[derive(Debug, Serialize, Error)]
pub struct JsonError {
    id: Option<i64>,
    jsonrpc: f32,
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

impl JsonResponse {
    pub fn new(id: i64, result: serde_json::Value) -> Self {
        Self {
            id,
            jsonrpc: 2.0,
            result,
        }
    }
}

impl JsonError {
    pub fn parse(data: Option<String>) -> Self {
        Self {
            id: None,
            jsonrpc: 2.0,
            code: JsonErrCode::Parse,
            message: "Parse error".to_string(),
            data,
        }
    }

    pub fn invalid_request(data: Option<String>) -> Self {
        Self {
            id: None,
            jsonrpc: 2.0,
            code: JsonErrCode::InvalidReq,
            message: "Invalid Request".to_string(),
            data,
        }
    }

    pub fn method_not_found(data: Option<String>) -> Self {
        Self {
            id: None,
            jsonrpc: 2.0,
            code: JsonErrCode::MethodNotFound,
            message: "Method not found".to_string(),
            data,
        }
    }

    pub fn invalid_param(data: Option<String>) -> Self {
        Self {
            id: None,
            jsonrpc: 2.0,
            code: JsonErrCode::InvalidParam,
            message: "Invalid params".to_string(),
            data,
        }
    }

    pub fn internal(data: Option<String>) -> Self {
        Self {
            id: None,
            jsonrpc: 2.0,
            code: JsonErrCode::Internal,
            message: "Internal jsonrpc error".to_string(),
            data,
        }
    }

    pub fn no_control(data: Option<String>) -> Self {
        Self {
            id: None,
            jsonrpc: 2.0,
            code: JsonErrCode::NoControl,
            message: "No player to control".to_string(),
            data,
        }
    }
}

impl Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
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
