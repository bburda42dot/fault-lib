/*
 * SPDX-License-Identifier: Apache-2.0
 * SPDX-FileCopyrightText: 2026 The Contributors to Eclipse OpenSOVD (see CONTRIBUTORS)
 *
 * See the NOTICE file(s) distributed with this work for additional
 * information regarding copyright ownership.
 *
 * This program and the accompanying materials are made available under the
 * terms of the Apache License Version 2.0 which is available at
 * https://www.apache.org/licenses/LICENSE-2.0
 */

//! E2E validation of the SDVOS body-service fault catalog.
//!
//! Loads the body-service JSON catalog, builds a DFM with query server,
//! reports faults, and queries them via IPC - validating the full pipeline
//! with the PoC catalog format.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::helpers::*;
use common::catalog::FaultCatalogBuilder;
use common::fault::{FaultId, LifecycleStage};
use common::types::to_static_short_string;
use dfm_lib::diagnostic_fault_manager::DiagnosticFaultManager;
use dfm_lib::fault_catalog_registry::FaultCatalogRegistry;
use dfm_lib::fault_record_processor::FaultRecordProcessor;
use dfm_lib::operation_cycle::OperationCycleTracker;
use dfm_lib::query_api::DfmQueryApi;
use dfm_lib::query_ipc::Iceoryx2DfmQuery;
use dfm_lib::sovd_fault_storage::KvsSovdFaultStateStorage;
use serial_test::serial;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// KVS instance counter (starts at 8 to avoid collisions with test_ipc_query which uses 2-7).
static BODY_KVS_INSTANCE: AtomicUsize = AtomicUsize::new(8);

const BODY_CATALOG_JSON: &str =
    include_str!("../../../../../sdvos/services/body-service/config/faults.json");

/// Validate that the body-service JSON catalog parses correctly.
#[test]
fn body_catalog_json_parses() {
    let catalog = FaultCatalogBuilder::new()
        .json_string(BODY_CATALOG_JSON)
        .expect("parse body catalog")
        .build();
    assert_eq!(catalog.id.as_ref(), "body");
    assert_eq!(catalog.len(), 8);
}

/// Full IPC E2E: report a fault via DFM, query it back via Iceoryx2DfmQuery.
#[test]
#[serial(ipc)]
fn body_catalog_ipc_report_and_query() {
    let instance_id = BODY_KVS_INSTANCE.fetch_add(1, Ordering::Relaxed);
    let dir = tempfile::TempDir::new().unwrap();

    let catalog = FaultCatalogBuilder::new()
        .json_string(BODY_CATALOG_JSON)
        .expect("parse")
        .build();
    let config = FaultCatalogBuilder::new()
        .json_string(BODY_CATALOG_JSON)
        .expect("parse")
        .build();

    let storage = Arc::new(KvsSovdFaultStateStorage::new(dir.path(), instance_id).unwrap());
    let registry = Arc::new(FaultCatalogRegistry::new(vec![catalog]));
    let cycle_tracker = Arc::new(RwLock::new(OperationCycleTracker::new()));

    // Report a fault via processor (simulating reporter side)
    {
        let mut processor =
            FaultRecordProcessor::new(Arc::clone(&storage), Arc::clone(&registry), cycle_tracker);
        let path = make_path("body");
        let fault_id = FaultId::Text(to_static_short_string("body.door.sensor_stuck").unwrap());
        let record = make_fault_record(fault_id, LifecycleStage::Failed);
        processor.process_record(&path, &record);
    }

    // Build DFM with query server
    drop(registry);
    let storage = Arc::try_unwrap(storage).ok().unwrap();
    let dfm_registry = FaultCatalogRegistry::new(vec![config]);
    let dfm = DiagnosticFaultManager::with_query_server(storage, dfm_registry);

    // Wait for query server
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match Iceoryx2DfmQuery::new() {
            Ok(_) => break,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("DFM query server not ready: {e}"),
        }
    }
    std::thread::sleep(Duration::from_millis(50));

    // Query via IPC
    let client = Iceoryx2DfmQuery::new().unwrap();
    let faults = client.get_all_faults("body").unwrap();
    assert_eq!(faults.len(), 8, "body catalog has 8 registered faults");

    let sensor_stuck = faults
        .iter()
        .find(|f| f.code == "body.door.sensor_stuck")
        .expect("reported fault should be queryable");

    assert_eq!(sensor_stuck.fault_name, "DoorSensorStuck");
    let status = sensor_stuck.typed_status.as_ref().unwrap();
    assert_eq!(status.test_failed, Some(true));
    assert_eq!(status.confirmed_dtc, Some(true));
    assert_eq!(sensor_stuck.occurrence_counter, Some(1));

    // Verify unreported faults have default status
    let lock_fault = faults
        .iter()
        .find(|f| f.code == "body.door.lock_failure")
        .expect("registered fault should be listed");
    let lock_status = lock_fault.typed_status.as_ref().unwrap();
    assert_eq!(lock_status.test_failed, Some(false));

    // Clear and verify
    client
        .delete_fault("body", "body.door.sensor_stuck")
        .unwrap();
    let (cleared, _) = client.get_fault("body", "body.door.sensor_stuck").unwrap();
    assert!(!cleared.typed_status.as_ref().unwrap().test_failed.unwrap());

    drop(dfm);
}
