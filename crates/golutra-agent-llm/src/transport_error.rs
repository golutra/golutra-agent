//! 根据传输层类型识别断网，禁止把 TLS 配置错误或正文关键词当作无限重连依据。

use std::error::Error;

pub(crate) fn has_permanent_transport_cause(error: &(dyn Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<rustls::Error>()
            || error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::InvalidData
                        | std::io::ErrorKind::InvalidInput
                        | std::io::ErrorKind::PermissionDenied
                )
            })
        {
            return true;
        }
        current = error.source();
    }
    false
}

pub(crate) fn genai_connection_failed(error: &genai::Error) -> bool {
    match error {
        genai::Error::WebAdapterCall { webc_error, .. }
        | genai::Error::WebModelCall { webc_error, .. } => webc_connection_failed(webc_error),
        genai::Error::WebStream { error, .. } => {
            error
                .downcast_ref::<genai::Error>()
                .is_some_and(genai_connection_failed)
                || error
                    .downcast_ref::<genai::webc::Error>()
                    .is_some_and(webc_connection_failed)
                || error
                    .downcast_ref::<reqwest13::Error>()
                    .is_some_and(|error| {
                        error.is_connect() && !has_permanent_transport_cause(error)
                    })
                || error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
                    error.is_connect() && !has_permanent_transport_cause(error)
                })
        }
        _ => false,
    }
}

pub(crate) fn genai_protocol_failure(error: &genai::Error) -> bool {
    match error {
        genai::Error::WebAdapterCall { webc_error, .. }
        | genai::Error::WebModelCall { webc_error, .. } => webc_protocol_failure(webc_error),
        genai::Error::WebStream { error, .. } => {
            error
                .downcast_ref::<genai::Error>()
                .is_some_and(genai_protocol_failure)
                || error
                    .downcast_ref::<genai::webc::Error>()
                    .is_some_and(webc_protocol_failure)
        }
        genai::Error::InvalidJsonResponseElement { .. }
        | genai::Error::ChatResponseGeneration { .. }
        | genai::Error::StreamParse { .. }
        | genai::Error::ChatResponse { .. }
        | genai::Error::SerdeJson(_) => true,
        _ => false,
    }
}

fn webc_protocol_failure(error: &genai::webc::Error) -> bool {
    matches!(
        error,
        genai::webc::Error::ResponseFailedNotJson { .. }
            | genai::webc::Error::ResponseFailedInvalidJson { .. }
    )
}

fn webc_connection_failed(error: &genai::webc::Error) -> bool {
    matches!(error, genai::webc::Error::Reqwest(error)
        if error.is_connect() && !has_permanent_transport_cause(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderError, genai_adapter::map_genai_error};

    fn wrapped(error: genai::webc::Error) -> genai::Error {
        genai::Error::WebStream {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAIResp, "fixture"),
            cause: error.to_string(),
            error: Box::new(error),
        }
    }

    #[tokio::test]
    async fn real_connection_refusal_survives_genai_stream_wrapping() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = reqwest13::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap_err();
        assert!(matches!(
            map_genai_error(wrapped(genai::webc::Error::Reqwest(error))),
            ProviderError::ConnectionFailed { .. }
        ));
    }

    #[test]
    fn html_error_page_with_connection_words_is_a_protocol_failure() {
        let error = wrapped(genai::webc::Error::ResponseFailedNotJson {
            content_type: "text/html".into(),
            body: "<html>connection stream reset</html>".into(),
        });
        assert!(matches!(
            map_genai_error(error),
            ProviderError::Malformed { .. }
        ));
    }

    #[test]
    fn invalid_tls_and_permission_errors_are_not_offline_recovery() {
        assert!(has_permanent_transport_cause(
            &rustls::Error::InvalidCertificate(rustls::CertificateError::Expired)
        ));
        assert!(has_permanent_transport_cause(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(!has_permanent_transport_cause(&std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        )));
    }
}
