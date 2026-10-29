//! Optional HTTP basic authentication, enabled with `--basic-auth`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum_extra::TypedHeader;
use axum_extra::headers::Authorization;
use axum_extra::headers::authorization::Basic;
use subtle::ConstantTimeEq;

/// The username and password every request must carry.
pub struct Credentials {
    username: String,
    password: String,
}

impl Credentials {
    /// Asks for the password on the terminal, so it doesn't end up in the shell's history.
    pub fn prompt(username: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !username.contains(':'),
            "the username can't contain a colon"
        );
        let password = rpassword::prompt_password(format!("Password for {username}: "))?;
        anyhow::ensure!(!password.is_empty(), "the password can't be empty");
        Ok(Self { username, password })
    }

    /// Compares in constant time, so response timing doesn't give away how much was right.
    fn matches(&self, given: &Basic) -> bool {
        let username = self.username.as_bytes().ct_eq(given.username().as_bytes());
        let password = self.password.as_bytes().ct_eq(given.password().as_bytes());
        (username & password).into()
    }
}

/// Middleware that turns away requests without the right credentials.
pub async fn require(
    State(credentials): State<Arc<Credentials>>,
    authorization: Option<TypedHeader<Authorization<Basic>>>,
    req: Request,
    next: Next,
) -> Response {
    match authorization {
        Some(TypedHeader(Authorization(given))) if credentials.matches(&given) => {
            next.run(req).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                r#"Basic realm="upserve", charset="UTF-8""#,
            )],
            "authentication required",
        )
            .into_response(),
    }
}
