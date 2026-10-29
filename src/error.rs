use std::io::{self, ErrorKind};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// An error that is sent back to the client as a plain-text response.
#[derive(Debug)]
pub struct Error {
    status: StatusCode,
    message: String,
}

impl Error {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    #[cfg(test)]
    pub fn status(&self) -> StatusCode {
        self.status
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        let status = match err.kind() {
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            ErrorKind::AlreadyExists
            | ErrorKind::NotADirectory
            | ErrorKind::IsADirectory
            | ErrorKind::DirectoryNotEmpty => StatusCode::CONFLICT,
            ErrorKind::InvalidInput | ErrorKind::ConnectionAborted => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, err.to_string())
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!("{}", self.message);
        }
        (self.status, self.message).into_response()
    }
}
