/*
 *  Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 *  SPDX-License-Identifier: Apache-2.0
 */

use crate::endpoint_lib::diagnostic::DiagnosticCollector;
use crate::endpoint_lib::host::is_valid_host_label;
use once_cell::sync::Lazy;
use regex::Regex;

static VIRTUAL_HOSTABLE_SEGMENT: Lazy<Regex> =
    Lazy::new(|| Regex::new("^[a-z\\d][a-z\\d\\-.]{1,61}[a-z\\d]$").unwrap());

static IPV4: Lazy<Regex> = Lazy::new(|| Regex::new("^(\\d+\\.){3}\\d+$").unwrap());

static DOTS_AND_DASHES: Lazy<Regex> = Lazy::new(|| Regex::new("^.*[.-]{2}.*$").unwrap());

// TODO(Zelda) add tests for this
/// Evaluates whether a string is a DNS-compatible bucket name that can be used with virtual hosted-style addressing.
pub(crate) fn is_virtual_hostable_s3_bucket(
    host_label: &str,
    allow_subdomains: bool,
    e: &mut DiagnosticCollector,
) -> bool {
    if !is_valid_host_label(host_label, allow_subdomains, e) {
        false
    } else if !allow_subdomains {
        is_virtual_hostable_segment(host_label)
    } else {
        host_label
            .split('.')
            .all(|segment| is_virtual_hostable_segment(segment))
    }
}

fn is_virtual_hostable_segment(host_label: &str) -> bool {
    VIRTUAL_HOSTABLE_SEGMENT.is_match(host_label)
        && !IPV4.is_match(host_label) // don't allow ip address
        && !DOTS_AND_DASHES.is_match(host_label) // don't allow names like bucket-.name or bucket.-name
}
