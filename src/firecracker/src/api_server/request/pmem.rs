// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use vmm::logger::{IncMetric, METRICS};
use vmm::rpc_interface::VmmAction;
use vmm::vmm_config::pmem::PmemDeviceConfig;

use super::super::parsed_request::{checked_id, ParsedRequest, RequestError};
use super::{Body, StatusCode};

pub(crate) fn parse_put_pmem(
    body: &Body,
    id_from_path: Option<&str>,
) -> Result<ParsedRequest, RequestError> {
    METRICS.put_api_requests.drive_count.inc();
    let id = if let Some(id) = id_from_path {
        checked_id(id)?
    } else {
        METRICS.put_api_requests.drive_fails.inc();
        return Err(RequestError::EmptyID);
    };

    let device_cfg = serde_json::from_slice::<PmemDeviceConfig>(body.raw()).inspect_err(|_| {
        METRICS.put_api_requests.drive_fails.inc();
    })?;

    if id != device_cfg.drive_id {
        METRICS.put_api_requests.drive_fails.inc();
        Err(RequestError::Generic(
            StatusCode::BadRequest,
            "The id from the path does not match the id from the body!".to_string(),
        ))
    } else {
        Ok(ParsedRequest::new_sync(VmmAction::InsertPmemDevice(
            device_cfg,
        )))
    }
}
