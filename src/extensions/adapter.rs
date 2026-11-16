//! Native Rust adapter trait (`DRIVER_NATIVE_ADAPTER`) reserved strictly for binary,
//! streaming, or heavy computational protocols (e.g., airborne LiDAR `.laz` point clouds)
//! that cannot be expressed as declarative YAML or OpenAPI 3.x HTTP connectors.

use crate::extensions::proto::{ExtensionEnvelopeProto, ExtensionManifestProto, ExtensionQueryProto};
use crate::shared::{EnvConfig, HttpClient};
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Pluggable trait for native Rust data extensions registered with `ExtensionHub`.
pub trait DataExtensionAdapter: Send + Sync + 'static {
    /// Returns the static manifest describing this native adapter's domain, priority, and contract.
    fn manifest(&self) -> ExtensionManifestProto;

    /// Optional synchronous execution hook for deterministic in-memory calculations.
    fn execute_sync(&self, _query: &ExtensionQueryProto) -> Result<Vec<ExtensionEnvelopeProto>> {
        Ok(Vec::new())
    }

    /// Asynchronous execution hook returning typed Protobuf envelopes with provenance citations.
    fn execute_async<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        _http: &'a HttpClient,
        _env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move { self.execute_sync(query) })
    }

    /// Synchronous execution hook receiving Stage-1/upstream `ExtensionEnvelopeProto` inputs.
    /// Defaults to forwarding `upstream` into `query.upstream_envelopes` if unset, then calling `execute_sync`.
    fn execute_with_upstream_sync(
        &self,
        query: &ExtensionQueryProto,
        upstream: &[ExtensionEnvelopeProto],
    ) -> Result<Vec<ExtensionEnvelopeProto>> {
        if query.upstream_envelopes.is_empty() && !upstream.is_empty() {
            let mut q = query.clone();
            q.upstream_envelopes = upstream.to_vec();
            self.execute_sync(&q)
        } else {
            self.execute_sync(query)
        }
    }

    /// Asynchronous execution hook receiving Stage-1/upstream `ExtensionEnvelopeProto` inputs.
    /// Defaults to forwarding `upstream` into `query.upstream_envelopes` if unset, then calling `execute_async`.
    fn execute_with_upstream<'a>(
        &'a self,
        query: &'a ExtensionQueryProto,
        upstream: &'a [ExtensionEnvelopeProto],
        http: &'a HttpClient,
        env: &'a EnvConfig,
    ) -> BoxFuture<'a, Result<Vec<ExtensionEnvelopeProto>>> {
        Box::pin(async move {
            if query.upstream_envelopes.is_empty() && !upstream.is_empty() {
                let mut q = query.clone();
                q.upstream_envelopes = upstream.to_vec();
                self.execute_async(&q, http, env).await
            } else {
                self.execute_async(query, http, env).await
            }
        })
    }
}
