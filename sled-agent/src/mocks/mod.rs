// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mock structures for testing.

use mockall::mock;
use nexus_client::types::{
    DatasetPutRequest, DatasetPutResponse, DiskRuntimeState,
    InstanceRuntimeState, SledAgentStartupInfo, UpdateArtifactKind,
    ZpoolPutRequest, ZpoolPutResponse,
};
use reqwest::Response;
use slog::Logger;
use uuid::Uuid;

type Result<T> = std::result::Result<
    T,
    progenitor::progenitor_client::Error<nexus_client::types::Error>,
>;

mock! {
    pub NexusClient {
        pub fn new(server_addr: &str, log: Logger) -> Self;
        pub fn client(&self) -> reqwest::Client;
        pub fn baseurl(&self) -> &'static str;
        pub async fn cpapi_sled_agents_post(
            &self,
            id: &Uuid,
            info: &SledAgentStartupInfo,
        ) -> Result<()>;
        pub async fn cpapi_instances_put(
            &self,
            id: &Uuid,
            new_runtime_state: &InstanceRuntimeState,
        ) -> Result<()>;
        pub async fn cpapi_disks_put(
            &self,
            disk_id: &Uuid,
            new_runtime_state: &DiskRuntimeState,
            ) -> Result<()>;
        pub async fn cpapi_artifact_download(
            &self,
            kind: UpdateArtifactKind,
            name: &str,
            version: i64,
        ) -> Result<Response>;
        pub async fn zpool_put(
            &self,
            sled_id: &Uuid,
            zpool_id: &Uuid,
            info: &ZpoolPutRequest,
        ) -> Result<ZpoolPutResponse>;
        pub async fn dataset_put(
            &self,
            zpool_id: &Uuid,
            dataset_id: &Uuid,
            info: &DatasetPutRequest,
        ) -> Result<DatasetPutResponse>;
    }
}
