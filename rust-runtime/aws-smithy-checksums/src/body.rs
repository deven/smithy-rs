/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

use crate::{new_checksum, Checksum};

use aws_smithy_http::body::SdkBody;
use aws_smithy_http::header::append_merge_header_maps;

use bytes::{Buf, Bytes};
use http::header::HeaderName;
use http::{HeaderMap, HeaderValue};
use http_body::{Body, SizeHint};
use pin_project::pin_project;

use std::fmt::Display;
use std::pin::Pin;
use std::task::{Context, Poll};

#[pin_project]
pub struct ChecksumBody<InnerBody> {
    #[pin]
    inner: InnerBody,
    #[pin]
    checksum: Box<dyn Checksum>,
}

impl ChecksumBody<SdkBody> {
    pub fn new(checksum_algorithm: &str, body: SdkBody) -> Self {
        Self {
            checksum: new_checksum(checksum_algorithm),
            inner: body,
        }
    }

    pub fn trailer_name(&self) -> HeaderName {
        self.checksum.header_name()
    }

    pub fn trailer_length(&self) -> u64 {
        self.checksum.checksum_header_size()
    }

    fn poll_inner(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, aws_smithy_http::body::Error>>> {
        let this = self.project();
        let inner = this.inner;
        let mut checksum = this.checksum;

        match inner.poll_data(cx) {
            Poll::Ready(Some(Ok(mut data))) => {
                let len = data.chunk().len();
                let bytes = data.copy_to_bytes(len);

                if let Err(e) = checksum.update(&bytes) {
                    return Poll::Ready(Some(Err(e)));
                }

                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl http_body::Body for ChecksumBody<SdkBody> {
    type Data = Bytes;
    type Error = aws_smithy_http::body::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        self.poll_inner(cx)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap<HeaderValue>>, Self::Error>> {
        let this = self.project();
        match (
            this.checksum.headers(),
            http_body::Body::poll_trailers(this.inner, cx),
        ) {
            // If everything is ready, return trailers, merging them if we have more than one map
            (Ok(outer_trailers), Poll::Ready(Ok(inner_trailers))) => {
                let trailers = match (outer_trailers, inner_trailers) {
                    // Values from the inner trailer map take precedent over values from the outer map
                    (Some(outer), Some(inner)) => Some(append_merge_header_maps(inner, outer)),
                    // If only one or neither produced trailers, just combine the `Option`s with `or`
                    (outer, inner) => outer.or(inner),
                };
                Poll::Ready(Ok(trailers))
            }
            // If the inner poll is Ok but the outer body's checksum callback encountered an error,
            // return the error
            (Err(e), Poll::Ready(Ok(_))) => Poll::Ready(Err(e)),
            // Otherwise return the result of the inner poll.
            // It may be pending or it may be ready with an error.
            (_, inner_poll) => inner_poll,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let body_size_hint = self.inner.size_hint();
        match body_size_hint.exact() {
            Some(size) => {
                let checksum_size_hint = self.checksum.size();
                SizeHint::with_exact(size + checksum_size_hint)
            }
            // TODO is this the right behavior?
            None => {
                let checksum_size_hint = self.checksum.size();
                let mut summed_size_hint = SizeHint::new();
                summed_size_hint.set_lower(body_size_hint.lower() + checksum_size_hint);

                if let Some(body_size_hint_upper) = body_size_hint.upper() {
                    summed_size_hint.set_upper(body_size_hint_upper + checksum_size_hint);
                }

                summed_size_hint
            }
        }
    }
}

#[pin_project]
pub struct ChecksumValidatedBody<InnerBody> {
    #[pin]
    inner: InnerBody,
    #[pin]
    checksum: Box<dyn Checksum>,
    precalculated_checksum: Bytes,
}

impl ChecksumValidatedBody<SdkBody> {
    pub fn new(body: SdkBody, checksum_algorithm: &str, precalculated_checksum: Bytes) -> Self {
        Self {
            checksum: new_checksum(checksum_algorithm),
            inner: body,
            precalculated_checksum,
        }
    }

    fn poll_inner(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, aws_smithy_http::body::Error>>> {
        let this = self.project();
        let inner = this.inner;
        let mut checksum = this.checksum;

        match inner.poll_data(cx) {
            Poll::Ready(Some(Ok(mut data))) => {
                let len = data.chunk().len();
                let bytes = data.copy_to_bytes(len);

                if let Err(e) = checksum.update(&bytes) {
                    return Poll::Ready(Some(Err(e)));
                }

                Poll::Ready(Some(Ok(bytes)))
            }
            // Once the inner body has stopped returning data, check the checksum
            // and return an error if it doesn't match.
            Poll::Ready(None) => {
                let actual_checksum = checksum.finalize();
                if *this.precalculated_checksum == actual_checksum {
                    Poll::Ready(None)
                } else {
                    // So many parens it's starting to look like LISP
                    Poll::Ready(Some(Err(Box::new(Error::checksum_mismatch(
                        this.precalculated_checksum.clone(),
                        actual_checksum,
                    )))))
                }
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Error {
    ChecksumMismatch { expected: Bytes, actual: Bytes },
}

impl Error {
    fn checksum_mismatch(expected: Bytes, actual: Bytes) -> Self {
        Self::ChecksumMismatch { expected, actual }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            Error::ChecksumMismatch { expected, actual } => write!(
                f,
                "body checksum mismatch. expected body checksum to be {:x} but it was {:x}",
                expected, actual
            ),
        }
    }
}

impl std::error::Error for Error {}

impl http_body::Body for ChecksumValidatedBody<SdkBody> {
    type Data = Bytes;
    type Error = aws_smithy_http::body::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        self.poll_inner(cx)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap<HeaderValue>>, Self::Error>> {
        self.project().inner.poll_trailers(cx)
    }

    // Once the inner body returns true for is_end_stream, we still need to
    // verify the checksum; Therefore, we always return false here.
    fn is_end_stream(&self) -> bool {
        false
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::ChecksumBody;
    use crate::{
        body::ChecksumValidatedBody, CRC_32_C_NAME, CRC_32_HEADER_NAME, CRC_32_NAME, SHA_1_NAME,
        SHA_256_NAME,
    };
    use aws_smithy_http::body::SdkBody;
    use aws_smithy_types::base64;
    use bytes::{Buf, Bytes};
    use bytes_utils::SegmentedBuf;
    use http_body::Body;
    use std::io::Read;

    fn header_value_as_checksum_string(header_value: &http::HeaderValue) -> String {
        let decoded_checksum = base64::decode(header_value.to_str().unwrap()).unwrap();
        let decoded_checksum = decoded_checksum
            .into_iter()
            .map(|byte| format!("{:02X?}", byte))
            .collect::<String>();

        format!("0x{}", decoded_checksum)
    }

    #[tokio::test]
    async fn test_checksum_body() {
        let input_text = "This is some test text for an SdkBody";
        let body = SdkBody::from(input_text);
        let mut body = ChecksumBody::new("crc32", body);

        let mut output = SegmentedBuf::new();
        while let Some(buf) = body.data().await {
            output.push(buf.unwrap());
        }

        let mut output_text = String::new();
        output
            .reader()
            .read_to_string(&mut output_text)
            .expect("Doesn't cause IO errors");
        // Verify data is complete and unaltered
        assert_eq!(input_text, output_text);

        let trailers = body
            .trailers()
            .await
            .expect("checksum generation was without error")
            .expect("trailers were set");
        let checksum_trailer = trailers
            .get(CRC_32_HEADER_NAME)
            .expect("trailers contain crc32 checksum");
        let checksum_trailer = header_value_as_checksum_string(checksum_trailer);

        // Known correct checksum for the input "This is some test text for an SdkBody"
        assert_eq!("0x99B01F72", checksum_trailer);
    }

    fn calculate_crc32_checksum(input: &str) -> Bytes {
        let checksum = crc32fast::hash(input.as_bytes());
        Bytes::copy_from_slice(&checksum.to_be_bytes())
    }

    #[tokio::test]
    async fn test_checksum_validated_body_errors_on_mismatch() {
        let input_text = "This is some test text for an SdkBody";
        let actual_checksum = calculate_crc32_checksum(input_text);
        let body = SdkBody::from(input_text);
        let non_matching_checksum = Bytes::copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        let mut body = ChecksumValidatedBody::new(body, "crc32", non_matching_checksum.clone());

        while let Some(data) = body.data().await {
            match data {
                Ok(_) => { /* Do nothing */ }
                Err(e) => {
                    let expected_error_message = format!(
                        "body checksum mismatch. expected body checksum to be {:x} but it was {:x}",
                        non_matching_checksum, actual_checksum
                    );
                    let actual_error_message = e.to_string();
                    assert_eq!(expected_error_message, actual_error_message);

                    return;
                }
            }
        }

        panic!("didn't hit expected error condition");
    }

    #[tokio::test]
    async fn test_checksum_validated_body_succeeds_on_match() {
        let input_text = "This is some test text for an SdkBody";
        let actual_checksum = calculate_crc32_checksum(input_text);
        let body = SdkBody::from(input_text);
        let mut body = ChecksumValidatedBody::new(body, "crc32", actual_checksum);

        let mut output = SegmentedBuf::new();
        while let Some(buf) = body.data().await {
            output.push(buf.unwrap());
        }

        let mut output_text = String::new();
        output
            .reader()
            .read_to_string(&mut output_text)
            .expect("Doesn't cause IO errors");
        // Verify data is complete and unaltered
        assert_eq!(input_text, output_text);
    }

    #[test]
    fn test_trailer_length_of_crc32_checksum_body() {
        let input_text = "Hello world";
        let body = ChecksumBody::new(CRC_32_NAME, SdkBody::from(input_text));
        let expected_size = 29;
        let actual_size = body.trailer_length();
        assert_eq!(expected_size, actual_size)
    }

    #[test]
    fn test_trailer_length_of_crc32c_checksum_body() {
        let input_text = "Hello world";
        let body = ChecksumBody::new(CRC_32_C_NAME, SdkBody::from(input_text));
        let expected_size = 30;
        let actual_size = body.trailer_length();
        assert_eq!(expected_size, actual_size)
    }

    #[test]
    fn test_trailer_length_of_sha1_checksum_body() {
        let input_text = "Hello world";
        let body = ChecksumBody::new(SHA_1_NAME, SdkBody::from(input_text));
        let expected_size = 48;
        let actual_size = body.trailer_length();
        assert_eq!(expected_size, actual_size)
    }

    #[test]
    fn test_trailer_length_of_sha256_checksum_body() {
        let input_text = "Hello world";
        let body = ChecksumBody::new(SHA_256_NAME, SdkBody::from(input_text));
        let expected_size = 66;
        let actual_size = body.trailer_length();
        assert_eq!(expected_size, actual_size)
    }
}
