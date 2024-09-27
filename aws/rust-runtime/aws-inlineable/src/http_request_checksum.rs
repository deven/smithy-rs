/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

#![allow(dead_code)]

//! Interceptor for handling Smithy `@httpChecksum` request checksumming with AWS SigV4

use aws_runtime::content_encoding::{AwsChunkedBody, AwsChunkedBodyOptions};
use aws_runtime::{auth::SigV4OperationSigningConfig, content_encoding::header_value::AWS_CHUNKED};
use aws_sigv4::http_request::SignableBody;
use aws_smithy_checksums::ChecksumAlgorithm;
use aws_smithy_checksums::{body::calculate, http::HttpChecksum};
use aws_smithy_runtime::client::sdk_feature::SmithySdkFeature;
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::interceptors::context::{
    BeforeSerializationInterceptorContextMut, BeforeTransmitInterceptorContextMut, Input,
};
use aws_smithy_runtime_api::client::interceptors::Intercept;
use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::checksum_config::RequestChecksumCalculation;
use aws_smithy_types::config_bag::{ConfigBag, Layer, Storable, StoreReplace};
use aws_smithy_types::error::operation::BuildError;
use http::HeaderValue;
use http_body::Body;
use std::{fmt, mem};

/// Errors related to constructing checksum-validated HTTP requests
#[derive(Debug)]
pub(crate) enum Error {
    /// Only request bodies with a known size can be checksum validated
    UnsizedRequestBody,
    ChecksumHeadersAreUnsupportedForStreamingBody,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsizedRequestBody => write!(
                f,
                "Only request bodies with a known size can be checksum validated."
            ),
            Self::ChecksumHeadersAreUnsupportedForStreamingBody => write!(
                f,
                "Checksum header insertion is only supported for non-streaming HTTP bodies. \
                   To checksum validate a streaming body, the checksums must be sent as trailers."
            ),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug)]
struct RequestChecksumInterceptorState {
    checksum_algorithm: Option<ChecksumAlgorithm>,
    /// This value is set in the model on the `httpChecksum` trait
    request_checksum_required: bool,
}
impl Storable for RequestChecksumInterceptorState {
    type Storer = StoreReplace<Self>;
}

type CustomDefaultFn = Box<
    dyn Fn(Option<ChecksumAlgorithm>, &ConfigBag) -> Option<ChecksumAlgorithm>
        + Send
        + Sync
        + 'static,
>;

pub(crate) struct DefaultRequestChecksumOverride {
    custom_default: CustomDefaultFn,
}
impl fmt::Debug for DefaultRequestChecksumOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefaultRequestChecksumOverride").finish()
    }
}
impl Storable for DefaultRequestChecksumOverride {
    type Storer = StoreReplace<Self>;
}
impl DefaultRequestChecksumOverride {
    pub(crate) fn new<F>(custom_default: F) -> Self
    where
        F: Fn(Option<ChecksumAlgorithm>, &ConfigBag) -> Option<ChecksumAlgorithm>
            + Send
            + Sync
            + 'static,
    {
        Self {
            custom_default: Box::new(custom_default),
        }
    }
    pub(crate) fn custom_default(
        &self,
        original: Option<ChecksumAlgorithm>,
        config_bag: &ConfigBag,
    ) -> Option<ChecksumAlgorithm> {
        (self.custom_default)(original, config_bag)
    }
}

pub(crate) struct RequestChecksumInterceptor<AP, CM> {
    algorithm_provider: AP,
    checksum_mutator: CM,
}

impl<AP, CM> fmt::Debug for RequestChecksumInterceptor<AP, CM> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestChecksumInterceptor").finish()
    }
}

impl<AP, CM> RequestChecksumInterceptor<AP, CM> {
    pub(crate) fn new(algorithm_provider: AP, checksum_mutator: CM) -> Self {
        Self {
            algorithm_provider,
            checksum_mutator,
        }
    }
}

impl<AP, CM> Intercept for RequestChecksumInterceptor<AP, CM>
where
    AP: Fn(&Input) -> Result<(Option<ChecksumAlgorithm>, bool), BoxError> + Send + Sync,
    CM: Fn(&mut Input, &ConfigBag) -> Result<(), BoxError> + Send + Sync,
{
    fn name(&self) -> &'static str {
        "RequestChecksumInterceptor"
    }

    fn modify_before_serialization(
        &self,
        context: &mut BeforeSerializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let _ = (self.checksum_mutator)(context.input_mut(), cfg);
        let checksum_algorithm = (self.algorithm_provider)(context.input())?;
        let mut layer = Layer::new("RequestChecksumInterceptor");
        layer.store_put(RequestChecksumInterceptorState {
            checksum_algorithm: checksum_algorithm.0,
            request_checksum_required: checksum_algorithm.1,
        });
        cfg.push_layer(layer);

        Ok(())
    }

    /// Calculate a checksum and modify the request to include the checksum as a header
    /// (for in-memory request bodies) or a trailer (for streaming request bodies).
    /// Streaming bodies must be sized or this will return an error.
    fn modify_before_signing(
        &self,
        context: &mut BeforeTransmitInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let state = cfg
            .load::<RequestChecksumInterceptorState>()
            .expect("set in `read_before_serialization`");

        // This value is from the trait, but is needed for runtime logic
        let request_checksum_required = state.request_checksum_required;

        // This value is set by the user on the SdkConfig to indicate their preference
        // We provide a default here for users that use a client config instead of the SdkConfig
        let request_checksum_calculation = cfg
            .load::<RequestChecksumCalculation>()
            .unwrap_or(&RequestChecksumCalculation::WhenSupported);

        // Determine if we actually calculate the checksum. If the user setting is WhenSupported (the default)
        // we always calculate it (because this interceptor isn't added if it isn't supported). If it is
        // WhenRequired we only calculate it if the checksum is marked required on the trait.
        let calculate_checksum = match request_checksum_calculation {
            RequestChecksumCalculation::WhenRequired => request_checksum_required,
            RequestChecksumCalculation::WhenSupported => true,
            _ => true,
        };

        // Calculate the checksum if necessary
        if calculate_checksum {
            // If a checksum override is set in the ConfigBag we use that instead (currently only used by S3Express)
            // If we have made it this far without a checksum being set we set the default as Crc32
            let checksum_algorithm = incorporate_custom_default(state.checksum_algorithm, cfg)
                .unwrap_or(ChecksumAlgorithm::Crc32);

            // Set the user-agent metric for the selected checksum algorithm
            match checksum_algorithm {
                ChecksumAlgorithm::Crc32 => {
                    cfg.interceptor_state()
                        .store_append(SmithySdkFeature::FlexibleChecksumsReqCrc32);
                }
                ChecksumAlgorithm::Crc32c => {
                    cfg.interceptor_state()
                        .store_append(SmithySdkFeature::FlexibleChecksumsReqCrc32c);
                }
                ChecksumAlgorithm::Md5 => {
                    tracing::warn!(more_info = "Unsupported ChecksumAlgorithm MD5 set");
                }
                ChecksumAlgorithm::Sha1 => {
                    cfg.interceptor_state()
                        .store_append(SmithySdkFeature::FlexibleChecksumsReqSha1);
                }
                ChecksumAlgorithm::Sha256 => {
                    cfg.interceptor_state()
                        .store_append(SmithySdkFeature::FlexibleChecksumsReqSha256);
                }
                unsupported => tracing::warn!(
                    more_info = "Unsupported value of ChecksumAlgorithm detected when setting user-agent metrics",
                    unsupported = ?unsupported),
            }

            let request = context.request_mut();
            add_checksum_for_request_body(request, checksum_algorithm, cfg)?;
        }

        Ok(())
    }

    /// Set the user-agent metrics for `RequestChecksumCalculation` here to avoid ownership issues
    /// with the mutable borrow of cfg in `modify_before_signing`
    fn read_after_serialization(
        &self,
        _context: &aws_smithy_runtime_api::client::interceptors::context::BeforeTransmitInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let request_checksum_calculation = cfg
            .load::<RequestChecksumCalculation>()
            .unwrap_or(&RequestChecksumCalculation::WhenSupported);

        match request_checksum_calculation {
            RequestChecksumCalculation::WhenSupported => {
                cfg.interceptor_state()
                    .store_append(SmithySdkFeature::FlexibleChecksumsReqWhenSupported);
            }
            RequestChecksumCalculation::WhenRequired => {
                cfg.interceptor_state()
                    .store_append(SmithySdkFeature::FlexibleChecksumsReqWhenRequired);
            }
            unsupported => tracing::warn!(
                    more_info = "Unsupported value of RequestChecksumCalculation when setting user-agent metrics",
                    unsupported = ?unsupported),
        };

        Ok(())
    }
}

fn incorporate_custom_default(
    checksum: Option<ChecksumAlgorithm>,
    cfg: &ConfigBag,
) -> Option<ChecksumAlgorithm> {
    match cfg.load::<DefaultRequestChecksumOverride>() {
        Some(checksum_override) => checksum_override.custom_default(checksum, cfg),
        None => checksum,
    }
}

fn add_checksum_for_request_body(
    request: &mut HttpRequest,
    checksum_algorithm: ChecksumAlgorithm,
    cfg: &mut ConfigBag,
) -> Result<(), BoxError> {
    match request.body().bytes() {
        // Body is in-memory: read it and insert the checksum as a header.
        Some(data) => {
            let mut checksum = checksum_algorithm.into_impl();

            // If the header has not already been set we set it. If it was already set by the user
            // we do nothing and maintain their set value.
            if request.headers().get(checksum.header_name()).is_none() {
                tracing::debug!("applying {checksum_algorithm:?} of the request body as a header");
                checksum.update(data);

                request
                    .headers_mut()
                    .insert(checksum.header_name(), checksum.header_value());
            }
        }
        // Body is streaming: wrap the body so it will emit a checksum as a trailer.
        None => {
            tracing::debug!("applying {checksum_algorithm:?} of the request body as a trailer");
            if let Some(mut signing_config) = cfg.load::<SigV4OperationSigningConfig>().cloned() {
                signing_config.signing_options.payload_override =
                    Some(SignableBody::StreamingUnsignedPayloadTrailer);
                cfg.interceptor_state().store_put(signing_config);
            }
            wrap_streaming_request_body_in_checksum_calculating_body(request, checksum_algorithm)?;
        }
    }
    Ok(())
}

fn wrap_streaming_request_body_in_checksum_calculating_body(
    request: &mut HttpRequest,
    checksum_algorithm: ChecksumAlgorithm,
) -> Result<(), BuildError> {
    let checksum = checksum_algorithm.into_impl();

    // If the user already set the header value then do nothing and return early
    if request.headers().get(checksum.header_name()).is_some() {
        return Ok(());
    }

    let original_body_size = request
        .body()
        .size_hint()
        .exact()
        .ok_or_else(|| BuildError::other(Error::UnsizedRequestBody))?;

    let mut body = {
        let body = mem::replace(request.body_mut(), SdkBody::taken());

        body.map(move |body| {
            let checksum = checksum_algorithm.into_impl();
            let trailer_len = HttpChecksum::size(checksum.as_ref());
            let body = calculate::ChecksumBody::new(body, checksum);
            let aws_chunked_body_options =
                AwsChunkedBodyOptions::new(original_body_size, vec![trailer_len]);

            let body = AwsChunkedBody::new(body, aws_chunked_body_options);

            SdkBody::from_body_0_4(body)
        })
    };

    let encoded_content_length = body
        .size_hint()
        .exact()
        .ok_or_else(|| BuildError::other(Error::UnsizedRequestBody))?;

    let headers = request.headers_mut();

    headers.insert(
        http::header::HeaderName::from_static("x-amz-trailer"),
        checksum.header_name(),
    );

    headers.insert(
        http::header::CONTENT_LENGTH,
        HeaderValue::from(encoded_content_length),
    );
    headers.insert(
        http::header::HeaderName::from_static("x-amz-decoded-content-length"),
        HeaderValue::from(original_body_size),
    );
    headers.insert(
        http::header::CONTENT_ENCODING,
        HeaderValue::from_str(AWS_CHUNKED)
            .map_err(BuildError::other)
            .expect("\"aws-chunked\" will always be a valid HeaderValue"),
    );

    mem::swap(request.body_mut(), &mut body);

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::http_request_checksum::wrap_streaming_request_body_in_checksum_calculating_body;
    use aws_smithy_checksums::ChecksumAlgorithm;
    use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
    use aws_smithy_types::base64;
    use aws_smithy_types::body::SdkBody;
    use aws_smithy_types::byte_stream::ByteStream;
    use bytes::BytesMut;
    use http_body::Body;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_checksum_body_is_retryable() {
        let input_text = "Hello world";
        let chunk_len_hex = format!("{:X}", input_text.len());
        let mut request: HttpRequest = http::Request::builder()
            .body(SdkBody::retryable(move || SdkBody::from(input_text)))
            .unwrap()
            .try_into()
            .unwrap();

        // ensure original SdkBody is retryable
        assert!(request.body().try_clone().is_some());

        let checksum_algorithm: ChecksumAlgorithm = "crc32".parse().unwrap();
        wrap_streaming_request_body_in_checksum_calculating_body(&mut request, checksum_algorithm)
            .unwrap();

        // ensure wrapped SdkBody is retryable
        let mut body = request.body().try_clone().expect("body is retryable");

        let mut body_data = BytesMut::new();
        while let Some(data) = body.data().await {
            body_data.extend_from_slice(&data.unwrap())
        }
        let body = std::str::from_utf8(&body_data).unwrap();
        assert_eq!(
            format!(
                "{chunk_len_hex}\r\n{input_text}\r\n0\r\nx-amz-checksum-crc32:i9aeUg==\r\n\r\n"
            ),
            body
        );
    }

    #[tokio::test]
    async fn test_checksum_body_from_file_is_retryable() {
        use std::io::Write;
        let mut file = NamedTempFile::new().unwrap();
        let checksum_algorithm: ChecksumAlgorithm = "crc32c".parse().unwrap();

        let mut crc32c_checksum = checksum_algorithm.into_impl();
        for i in 0..10000 {
            let line = format!("This is a large file created for testing purposes {}", i);
            file.as_file_mut().write_all(line.as_bytes()).unwrap();
            crc32c_checksum.update(line.as_bytes());
        }
        let crc32c_checksum = crc32c_checksum.finalize();

        let mut request = HttpRequest::new(
            ByteStream::read_from()
                .path(&file)
                .buffer_size(1024)
                .build()
                .await
                .unwrap()
                .into_inner(),
        );

        // ensure original SdkBody is retryable
        assert!(request.body().try_clone().is_some());

        wrap_streaming_request_body_in_checksum_calculating_body(&mut request, checksum_algorithm)
            .unwrap();

        // ensure wrapped SdkBody is retryable
        let mut body = request.body().try_clone().expect("body is retryable");

        let mut body_data = BytesMut::new();
        while let Some(data) = body.data().await {
            body_data.extend_from_slice(&data.unwrap())
        }
        let body = std::str::from_utf8(&body_data).unwrap();
        let expected_checksum = base64::encode(&crc32c_checksum);
        let expected = format!("This is a large file created for testing purposes 9999\r\n0\r\nx-amz-checksum-crc32c:{expected_checksum}\r\n\r\n");
        assert!(
            body.ends_with(&expected),
            "expected {body} to end with '{expected}'"
        );
    }
}
