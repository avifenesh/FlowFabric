// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

use crate::value::ValkeyError;

#[derive(Debug, Clone, PartialEq)]
pub enum RequestErrorType {
    Unspecified = 0,
    ExecAbort = 1,
    Timeout = 2,
    Disconnect = 3,
}

pub fn error_type(error: &ValkeyError) -> RequestErrorType {
    if error.is_timeout() {
        RequestErrorType::Timeout
    } else if error.is_unrecoverable_error() {
        RequestErrorType::Disconnect
    } else if matches!(error.kind(), crate::value::ErrorKind::ExecAbortError) {
        RequestErrorType::ExecAbort
    } else {
        RequestErrorType::Unspecified
    }
}

pub fn error_message(error: &ValkeyError) -> String {
    let error_message = error.to_string();
    if matches!(error_type(error), RequestErrorType::Disconnect) {
        format!("Received connection error `{error_message}`. Will attempt to reconnect")
    } else {
        error_message
    }
}
