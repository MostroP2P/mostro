//! Bearer-token authentication for the admin gRPC service (issue #807).

use secrecy::{ExposeSecret, SecretString};
use subtle::ConstantTimeEq;
use tonic::service::Interceptor;
use tonic::{Request, Status};

const AUTH_HEADER: &str = "authorization";
const BEARER_PREFIX: &str = "Bearer ";

/// Rejects any admin RPC call whose `authorization: Bearer <token>` metadata
/// does not match the configured token. The byte comparison is constant-time
/// (`subtle::ConstantTimeEq`) so a timing side channel can't be used to guess
/// the token; the length check that gates it is not constant-time, but the
/// length of a caller-supplied token isn't secret information.
#[derive(Clone)]
pub struct TokenAuthInterceptor {
    token: SecretString,
}

impl TokenAuthInterceptor {
    pub fn new(token: SecretString) -> Self {
        Self { token }
    }
}

impl Interceptor for TokenAuthInterceptor {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let provided = request
            .metadata()
            .get(AUTH_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix(BEARER_PREFIX));

        let Some(provided) = provided else {
            return Err(Status::unauthenticated(
                "missing or malformed authorization header",
            ));
        };

        let matches: bool = provided
            .as_bytes()
            .ct_eq(self.token.expose_secret().as_bytes())
            .into();
        if matches {
            Ok(request)
        } else {
            Err(Status::unauthenticated("invalid admin RPC token"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interceptor(token: &str) -> TokenAuthInterceptor {
        TokenAuthInterceptor::new(SecretString::from(token.to_string()))
    }

    fn request_with_auth(value: Option<&str>) -> Request<()> {
        let mut request = Request::new(());
        if let Some(value) = value {
            request
                .metadata_mut()
                .insert(AUTH_HEADER, value.parse().unwrap());
        }
        request
    }

    #[test]
    fn accepts_valid_token() {
        let mut interceptor = interceptor("s3cr3t");
        let result = interceptor.call(request_with_auth(Some("Bearer s3cr3t")));
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_missing_header() {
        let mut interceptor = interceptor("s3cr3t");
        let err = interceptor.call(request_with_auth(None)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_malformed_header() {
        let mut interceptor = interceptor("s3cr3t");
        let err = interceptor
            .call(request_with_auth(Some("s3cr3t")))
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_wrong_token() {
        let mut interceptor = interceptor("s3cr3t");
        let err = interceptor
            .call(request_with_auth(Some("Bearer wrong")))
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn rejects_wrong_length_token() {
        let mut interceptor = interceptor("s3cr3t");
        let err = interceptor
            .call(request_with_auth(Some("Bearer s3cr3t-but-longer")))
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }
}
