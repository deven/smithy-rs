/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

use std::error::Error;
use std::fmt;

/// A checksum algorithm was unknown
#[derive(Debug)]
pub struct UnknownChecksumAlgorithmError {
    checksum_algorithm: String,
}

impl UnknownChecksumAlgorithmError {
    pub(crate) fn new(checksum_algorithm: impl Into<String>) -> Self {
        Self {
            checksum_algorithm: checksum_algorithm.into(),
        }
    }

    /// The checksum algorithm that is unknown
    pub fn checksum_algorithm(&self) -> &str {
        &self.checksum_algorithm
    }
}

impl fmt::Display for UnknownChecksumAlgorithmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            r#"unknown checksum algorithm "{}", please pass a known algorithm name ("crc32", "crc32c", "sha1", "sha256", "md5")"#,
            self.checksum_algorithm
        )
    }
}

impl Error for UnknownChecksumAlgorithmError {}

/// Unknown setting for `request_checksum_calculation`
#[derive(Debug)]
pub struct UnknownRequestChecksumCalculationError {
    request_checksum_calculation: String,
}

impl UnknownRequestChecksumCalculationError {
    pub(crate) fn new(request_checksum_calculation: impl Into<String>) -> Self {
        Self {
            request_checksum_calculation: request_checksum_calculation.into(),
        }
    }

    /// The unknown value
    pub fn request_checksum_calculation(&self) -> &str {
        &self.request_checksum_calculation
    }
}

impl fmt::Display for UnknownRequestChecksumCalculationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            r#"unknown request_checksum_calculation value "{}", please pass a known name ("when_supported", "when_required")"#,
            self.request_checksum_calculation
        )
    }
}

impl Error for UnknownRequestChecksumCalculationError {}

/// Unknown setting for `response_checksum_validation`
#[derive(Debug)]
pub struct UnknownResponseChecksumValidationError {
    response_checksum_validation: String,
}

impl UnknownResponseChecksumValidationError {
    pub(crate) fn new(response_checksum_validation: impl Into<String>) -> Self {
        Self {
            response_checksum_validation: response_checksum_validation.into(),
        }
    }

    /// The unknown value
    pub fn response_checksum_validation(&self) -> &str {
        &self.response_checksum_validation
    }
}

impl fmt::Display for UnknownResponseChecksumValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            r#"unknown response_checksum_validation value "{}", please pass a known name ("when_supported", "when_required")"#,
            self.response_checksum_validation
        )
    }
}

impl Error for UnknownResponseChecksumValidationError {}
