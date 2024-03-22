//! The client module for sending requests and parsing responses

use crate::error::Error;
use crate::error::Error::ResponseError;
use crate::signer::Signer;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_rustls::{ConfigBuilderExt, HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::request::payload::Payload;
use crate::response::Response;
use http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use hyper::{self, StatusCode};
use hyper_util::client::legacy::Client as HttpClient;
use std::convert::Infallible;
use std::io::Read;
use std::time::Duration;
use std::{fmt, io};

type HyperConnector = HttpsConnector<HttpConnector>;

/// The APNs service endpoint to connect.
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// The production environment (api.push.apple.com)
    Production,
    /// The development/test environment (api.development.push.apple.com)
    Sandbox,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = match self {
            Endpoint::Production => "api.push.apple.com",
            Endpoint::Sandbox => "api.development.push.apple.com",
        };

        write!(f, "{}", host)
    }
}

/// Handles requests to and responses from Apple Push Notification service.
/// Connects using a given connector. Handles the needed authentication and
/// maps responses.
///
/// The `send` method returns a future, which is successful when APNs receives
/// the notification and responds with a status OK. In any other case the future
/// fails. If APNs gives a reason for the failure, the returned `Err`
/// holds the response for handling.
#[derive(Debug, Clone)]
pub struct Client {
    endpoint: Endpoint,
    signer: Option<Signer>,
    http_client: HttpClient<HyperConnector, BoxBody<Bytes, Infallible>>,
}

impl Client {
    fn new(connector: HyperConnector, signer: Option<Signer>, endpoint: Endpoint) -> Client {
        let mut builder = HttpClient::builder(TokioExecutor::new());
        builder
            .pool_idle_timeout(Some(Duration::from_secs(600)))
            .http2_only(true);

        Client {
            http_client: builder.build(connector),
            signer,
            endpoint,
        }
    }

    /// Create a connection to APNs using the provider client certificate which
    /// you obtain from your [Apple developer
    /// account](https://developer.apple.com/account/).
    ///
    /// Only works with the `openssl` feature.
    #[cfg(feature = "openssl")]
    pub fn certificate<R>(certificate: &mut R, password: &str, endpoint: Endpoint) -> Result<Client, Error>
    where
        R: Read,
    {
        let mut cert_der: Vec<u8> = Vec::new();
        certificate.read_to_end(&mut cert_der)?;

        let pkcs = openssl::pkcs12::Pkcs12::from_der(&cert_der)?.parse2(password)?;
        let Some((cert, pkey)) = pkcs.cert.zip(pkcs.pkey) else {
            return Err(Error::InvalidCertificate);
        };
        let connector = client_cert_connector(&cert.to_pem()?, &pkey.private_key_to_pem_pkcs8()?)?;

        Ok(Self::new(connector, None, endpoint))
    }

    /// Create a connection to APNs using system certificates, signing every
    /// request with a signature using a private key, key id and team id
    /// provisioned from your [Apple developer
    /// account](https://developer.apple.com/account/).
    pub fn token<S, T, R>(pkcs8_pem: R, key_id: S, team_id: T, endpoint: Endpoint) -> Result<Client, Error>
    where
        S: Into<String>,
        T: Into<String>,
        R: Read,
    {
        let connector = default_connector();
        let signature_ttl = Duration::from_secs(60 * 55);
        let signer = Signer::new(pkcs8_pem, key_id, team_id, signature_ttl)?;

        Ok(Self::new(connector, Some(signer), endpoint))
    }

    /// Send a notification payload.
    ///
    /// See [ErrorReason](enum.ErrorReason.html) for possible errors.
    #[cfg_attr(feature = "tracing", ::tracing::instrument)]
    pub async fn send(&self, payload: Payload<'_>) -> Result<Response, Error> {
        let request = self.build_request(payload);
        let requesting = self.http_client.request(request);

        let response = requesting.await?;

        let apns_id = response
            .headers()
            .get("apns-id")
            .and_then(|s| s.to_str().ok())
            .map(String::from);

        match response.status() {
            StatusCode::OK => Ok(Response {
                apns_id,
                error: None,
                code: response.status().as_u16(),
            }),
            status => {
                let body = response.into_body().collect().await?;

                Err(ResponseError(Response {
                    apns_id,
                    error: serde_json::from_slice(&body.to_bytes()).ok(),
                    code: status.as_u16(),
                }))
            }
        }
    }

    fn build_request(&self, payload: Payload<'_>) -> hyper::Request<BoxBody<Bytes, Infallible>> {
        let path = format!("https://{}/3/device/{}", self.endpoint, payload.device_token);

        let mut builder = hyper::Request::builder()
            .uri(&path)
            .method("POST")
            .header(CONTENT_TYPE, "application/json");

        if let Some(ref apns_priority) = payload.options.apns_priority {
            builder = builder.header("apns-priority", apns_priority.to_string().as_bytes());
        }
        if let Some(apns_id) = payload.options.apns_id {
            builder = builder.header("apns-id", apns_id.as_bytes());
        }
        if let Some(ref apns_expiration) = payload.options.apns_expiration {
            builder = builder.header("apns-expiration", apns_expiration.to_string().as_bytes());
        }
        if let Some(ref apns_collapse_id) = payload.options.apns_collapse_id {
            builder = builder.header("apns-collapse-id", apns_collapse_id.value.to_string().as_bytes());
        }
        if let Some(apns_topic) = payload.options.apns_topic {
            builder = builder.header("apns-topic", apns_topic.as_bytes());
        }
        if let Some(ref signer) = self.signer {
            let auth = signer
                .with_signature(|signature| format!("Bearer {}", signature))
                .unwrap();

            builder = builder.header(AUTHORIZATION, auth.as_bytes());
        }

        let payload_json = payload.to_json_string().unwrap().into_bytes();
        builder = builder.header(CONTENT_LENGTH, format!("{}", payload_json.len()).as_bytes());

        let request_body = Full::from(Bytes::from(payload_json)).boxed();
        builder.body(request_body).unwrap()
    }
}

fn default_connector() -> HyperConnector {
    HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http2()
        .build()
}

fn client_cert_connector(mut cert_pem: &[u8], mut key_pem: &[u8]) -> Result<HyperConnector, Error> {
    let private_key_error = || io::Error::new(io::ErrorKind::InvalidData, "private key");

    let key = rustls_pemfile::pkcs8_private_keys(&mut key_pem)
        .next()
        .ok_or_else(private_key_error)?
        .map_err(|_| private_key_error())?;

    let cert_chain: Result<Vec<_>, _> = rustls_pemfile::certs(&mut cert_pem).collect();
    let cert_chain = cert_chain.map_err(|_| private_key_error())?;

    let config = rustls::client::ClientConfig::builder()
        .with_webpki_roots()
        .with_client_auth_cert(cert_chain, key.into())?;

    Ok(HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_only()
        .enable_http2()
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::notification::NotificationBuilder;
    use crate::request::notification::PlainNotificationBuilder;
    use crate::request::notification::{CollapseId, NotificationOptions, Priority};
    use crate::signer::Signer;
    use http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
    use hyper::Method;

    const PRIVATE_KEY: &'static str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg8g/n6j9roKvnUkwu
lCEIvbDqlUhA5FOzcakkG90E8L+hRANCAATKS2ZExEybUvchRDuKBftotMwVEus3
jDwmlD1Gg0yJt1e38djFwsxsfr5q2hv0Rj9fTEqAPr8H7mGm0wKxZ7iQ
-----END PRIVATE KEY-----";

    #[test]
    fn test_production_request_uri() {
        let builder = PlainNotificationBuilder::new("test");
        let payload = builder.build("a_test_id", Default::default());
        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let uri = format!("{}", request.uri());

        assert_eq!("https://api.push.apple.com/3/device/a_test_id", &uri);
    }

    #[test]
    fn test_sandbox_request_uri() {
        let builder = PlainNotificationBuilder::new("test");
        let payload = builder.build("a_test_id", Default::default());
        let client = Client::new(default_connector(), None, Endpoint::Sandbox);
        let request = client.build_request(payload);
        let uri = format!("{}", request.uri());

        assert_eq!("https://api.development.push.apple.com/3/device/a_test_id", &uri);
    }

    #[test]
    fn test_request_method() {
        let builder = PlainNotificationBuilder::new("test");
        let payload = builder.build("a_test_id", Default::default());
        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);

        assert_eq!(&Method::POST, request.method());
    }

    #[test]
    fn test_request_content_type() {
        let builder = PlainNotificationBuilder::new("test");
        let payload = builder.build("a_test_id", Default::default());
        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);

        assert_eq!("application/json", request.headers().get(CONTENT_TYPE).unwrap());
    }

    #[test]
    fn test_request_content_length() {
        let builder = PlainNotificationBuilder::new("test");
        let payload = builder.build("a_test_id", Default::default());
        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload.clone());
        let payload_json = payload.to_json_string().unwrap();
        let content_length = request.headers().get(CONTENT_LENGTH).unwrap().to_str().unwrap();

        assert_eq!(&format!("{}", payload_json.len()), content_length);
    }

    #[test]
    fn test_request_authorization_with_no_signer() {
        let builder = PlainNotificationBuilder::new("test");
        let payload = builder.build("a_test_id", Default::default());
        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);

        assert_eq!(None, request.headers().get(AUTHORIZATION));
    }

    #[test]
    fn test_request_authorization_with_a_signer() {
        let signer = Signer::new(
            PRIVATE_KEY.as_bytes(),
            "89AFRD1X22",
            "ASDFQWERTY",
            Duration::from_secs(100),
        )
        .unwrap();

        let builder = PlainNotificationBuilder::new("test");
        let payload = builder.build("a_test_id", Default::default());
        let client = Client::new(default_connector(), Some(signer), Endpoint::Production);
        let request = client.build_request(payload);

        assert_ne!(None, request.headers().get(AUTHORIZATION));
    }

    #[test]
    fn test_request_with_default_priority() {
        let builder = PlainNotificationBuilder::new("test");
        let payload = builder.build("a_test_id", Default::default());
        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_priority = request.headers().get("apns-priority");

        assert_eq!(None, apns_priority);
    }

    #[test]
    fn test_request_with_normal_priority() {
        let builder = PlainNotificationBuilder::new("test");

        let payload = builder.build(
            "a_test_id",
            NotificationOptions {
                apns_priority: Some(Priority::Normal),
                ..Default::default()
            },
        );

        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_priority = request.headers().get("apns-priority").unwrap();

        assert_eq!("5", apns_priority);
    }

    #[test]
    fn test_request_with_high_priority() {
        let builder = PlainNotificationBuilder::new("test");

        let payload = builder.build(
            "a_test_id",
            NotificationOptions {
                apns_priority: Some(Priority::High),
                ..Default::default()
            },
        );

        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_priority = request.headers().get("apns-priority").unwrap();

        assert_eq!("10", apns_priority);
    }

    #[test]
    fn test_request_with_default_apns_id() {
        let builder = PlainNotificationBuilder::new("test");

        let payload = builder.build("a_test_id", Default::default());

        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_id = request.headers().get("apns-id");

        assert_eq!(None, apns_id);
    }

    #[test]
    fn test_request_with_an_apns_id() {
        let builder = PlainNotificationBuilder::new("test");

        let payload = builder.build(
            "a_test_id",
            NotificationOptions {
                apns_id: Some("a-test-apns-id"),
                ..Default::default()
            },
        );

        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_id = request.headers().get("apns-id").unwrap();

        assert_eq!("a-test-apns-id", apns_id);
    }

    #[test]
    fn test_request_with_default_apns_expiration() {
        let builder = PlainNotificationBuilder::new("test");

        let payload = builder.build("a_test_id", Default::default());

        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_expiration = request.headers().get("apns-expiration");

        assert_eq!(None, apns_expiration);
    }

    #[test]
    fn test_request_with_an_apns_expiration() {
        let builder = PlainNotificationBuilder::new("test");

        let payload = builder.build(
            "a_test_id",
            NotificationOptions {
                apns_expiration: Some(420),
                ..Default::default()
            },
        );

        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_expiration = request.headers().get("apns-expiration").unwrap();

        assert_eq!("420", apns_expiration);
    }

    #[test]
    fn test_request_with_default_apns_collapse_id() {
        let builder = PlainNotificationBuilder::new("test");

        let payload = builder.build("a_test_id", Default::default());

        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_collapse_id = request.headers().get("apns-collapse-id");

        assert_eq!(None, apns_collapse_id);
    }

    #[test]
    fn test_request_with_an_apns_collapse_id() {
        let builder = PlainNotificationBuilder::new("test");

        let payload = builder.build(
            "a_test_id",
            NotificationOptions {
                apns_collapse_id: Some(CollapseId::new("a_collapse_id").unwrap()),
                ..Default::default()
            },
        );

        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_collapse_id = request.headers().get("apns-collapse-id").unwrap();

        assert_eq!("a_collapse_id", apns_collapse_id);
    }

    #[test]
    fn test_request_with_default_apns_topic() {
        let builder = PlainNotificationBuilder::new("test");

        let payload = builder.build("a_test_id", Default::default());

        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_topic = request.headers().get("apns-topic");

        assert_eq!(None, apns_topic);
    }

    #[test]
    fn test_request_with_an_apns_topic() {
        let builder = PlainNotificationBuilder::new("test");

        let payload = builder.build(
            "a_test_id",
            NotificationOptions {
                apns_topic: Some("a_topic"),
                ..Default::default()
            },
        );

        let client = Client::new(default_connector(), None, Endpoint::Production);
        let request = client.build_request(payload);
        let apns_topic = request.headers().get("apns-topic").unwrap();

        assert_eq!("a_topic", apns_topic);
    }

    // #[tokio::test]
    // async fn test_request_body() {
    //     let builder = PlainNotificationBuilder::new("test");
    //     let payload = builder.build("a_test_id", Default::default());
    //     let client = Client::new(default_connector(), None, Endpoint::Production);
    //     let request = client.build_request(payload.clone());
    //
    //     let body = hyper::body::to_bytes(request).await.unwrap();
    //     let body_str = String::from_utf8(body.to_vec()).unwrap();
    //
    //     assert_eq!(payload.to_json_string().unwrap(), body_str,);
    // }
}
