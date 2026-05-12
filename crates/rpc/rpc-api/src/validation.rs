//! API for block submission validation.

use alloy_eips::{eip4844::kzg_to_versioned_hash, eip7685::RequestsOrHash};
use alloy_primitives::B256;
use alloy_rpc_types_beacon::{
    relay::{
        BidTrace, BuilderBlockValidationRequest, BuilderBlockValidationRequestV2,
        BuilderBlockValidationRequestV3, BuilderBlockValidationRequestV4,
        BuilderBlockValidationRequestV5,
    },
    requests::ExecutionRequestsV4,
    BlsSignature,
};
use alloy_rpc_types_engine::{
    BlobsBundleV2, CancunPayloadFields, ExecutionData, ExecutionPayload, ExecutionPayloadSidecar,
    ExecutionPayloadV4, PraguePayloadFields,
};
use jsonrpsee::proc_macros::rpc;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Block validation rpc interface.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "flashbots"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "flashbots"))]
pub trait BlockSubmissionValidationApi {
    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV1")]
    async fn validate_builder_submission_v1(
        &self,
        request: BuilderBlockValidationRequest,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV2")]
    async fn validate_builder_submission_v2(
        &self,
        request: BuilderBlockValidationRequestV2,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV3")]
    async fn validate_builder_submission_v3(
        &self,
        request: BuilderBlockValidationRequestV3,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV4")]
    async fn validate_builder_submission_v4(
        &self,
        request: BuilderBlockValidationRequestV4,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV5")]
    async fn validate_builder_submission_v5(
        &self,
        request: BuilderBlockValidationRequestV5,
    ) -> jsonrpsee::core::RpcResult<()>;

    /// A Request to validate a block submission.
    #[method(name = "validateBuilderSubmissionV6")]
    async fn validate_builder_submission_v6(
        &self,
        request: BuilderBlockValidationRequestV6,
    ) -> jsonrpsee::core::RpcResult<()>;
}

/// Submission for the `/relay/v1/builder/blocks` endpoint (Amsterdam).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionV6 {
    /// The [`BidTrace`] message associated with the submission.
    pub message: BidTrace,
    /// The execution payload for the submission.
    #[serde(with = "alloy_rpc_types_beacon::payload::beacon_payload_v4")]
    pub execution_payload: ExecutionPayloadV4,
    /// The blobs bundle associated with this bid.
    pub blobs_bundle: BlobsBundleV2,
    /// The execution requests associated with this bid.
    pub execution_requests: ExecutionRequestsV4,
    /// The signature associated with the submission.
    pub signature: BlsSignature,
}

/// A Request to validate a [`SignedBidSubmissionV6`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderBlockValidationRequestV6 {
    /// The request to be validated.
    #[serde(flatten)]
    pub request: SignedBidSubmissionV6,
    /// The registered gas limit for the validation request.
    #[serde(with = "decimal_string_u64")]
    pub registered_gas_limit: u64,
    /// The parent beacon block root for the validation request.
    pub parent_beacon_block_root: B256,
}

impl BuilderBlockValidationRequestV6 {
    /// Converts this validation request to [`ExecutionData`].
    ///
    /// Extracts the Amsterdam execution payload and creates the appropriate sidecar with
    /// versioned hashes and execution requests.
    pub fn into_execution_data(self) -> ExecutionData {
        let versioned_hashes = self
            .request
            .blobs_bundle
            .commitments
            .iter()
            .map(|commitment| kzg_to_versioned_hash(commitment.as_slice()))
            .collect();

        let cancun_fields = CancunPayloadFields {
            parent_beacon_block_root: self.parent_beacon_block_root,
            versioned_hashes,
        };
        let prague_fields = PraguePayloadFields {
            requests: RequestsOrHash::Requests(self.request.execution_requests.to_requests()),
        };
        let sidecar = ExecutionPayloadSidecar::v4(cancun_fields, prague_fields);

        ExecutionData::new(ExecutionPayload::V4(self.request.execution_payload), sidecar)
    }
}

impl From<BuilderBlockValidationRequestV6> for ExecutionData {
    fn from(request: BuilderBlockValidationRequestV6) -> Self {
        request.into_execution_data()
    }
}

mod decimal_string_u64 {
    use super::*;

    pub(crate) fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(value)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = u64;

            fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                formatter.write_str("a decimal string or integer")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(value)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}
