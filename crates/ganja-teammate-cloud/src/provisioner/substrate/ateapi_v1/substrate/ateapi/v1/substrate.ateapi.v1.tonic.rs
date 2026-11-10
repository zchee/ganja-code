// @generated
/// Generated client implementations.
pub mod control_client {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct ControlClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl ControlClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> ControlClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::Body>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + std::marker::Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + std::marker::Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> ControlClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::Body>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::Body>,
            >>::Error: Into<StdError> + std::marker::Send + std::marker::Sync,
        {
            ControlClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        pub async fn get_actor(
            &mut self,
            request: impl tonic::IntoRequest<super::GetActorRequest>,
        ) -> std::result::Result<tonic::Response<super::Actor>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/GetActor",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "GetActor"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_actor(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateActorRequest>,
        ) -> std::result::Result<tonic::Response<super::Actor>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/CreateActor",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "CreateActor"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn update_actor(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateActorRequest>,
        ) -> std::result::Result<tonic::Response<super::Actor>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/UpdateActor",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "UpdateActor"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn suspend_actor(
            &mut self,
            request: impl tonic::IntoRequest<super::SuspendActorRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SuspendActorResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/SuspendActor",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "SuspendActor"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn resume_actor(
            &mut self,
            request: impl tonic::IntoRequest<super::ResumeActorRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ResumeActorResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/ResumeActor",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "ResumeActor"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn revert_actor(
            &mut self,
            request: impl tonic::IntoRequest<super::RevertActorRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RevertActorResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/RevertActor",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "RevertActor"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn delete_actor(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteActorRequest>,
        ) -> std::result::Result<tonic::Response<super::Actor>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/DeleteActor",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "DeleteActor"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_actor_egress_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::GetActorEgressPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::EgressPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/GetActorEgressPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "GetActorEgressPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_actor_egress_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateActorEgressPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::EgressPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/CreateActorEgressPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "CreateActorEgressPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn update_actor_egress_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateActorEgressPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::EgressPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/UpdateActorEgressPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "UpdateActorEgressPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn delete_actor_egress_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteActorEgressPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::EgressPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/DeleteActorEgressPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "DeleteActorEgressPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn mint_actor_jwt(
            &mut self,
            request: impl tonic::IntoRequest<super::MintActorJwtRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MintActorJwtResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/MintActorJwt",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "MintActorJwt"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_tag(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateTagRequest>,
        ) -> std::result::Result<tonic::Response<super::Tag>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/CreateTag",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "CreateTag"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_tag(
            &mut self,
            request: impl tonic::IntoRequest<super::GetTagRequest>,
        ) -> std::result::Result<tonic::Response<super::Tag>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/GetTag",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "GetTag"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_tags(
            &mut self,
            request: impl tonic::IntoRequest<super::ListTagsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListTagsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/ListTags",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "ListTags"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn update_tag(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateTagRequest>,
        ) -> std::result::Result<tonic::Response<super::Tag>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/UpdateTag",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "UpdateTag"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn delete_tag(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteTagRequest>,
        ) -> std::result::Result<tonic::Response<super::Tag>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/DeleteTag",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "DeleteTag"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_workers(
            &mut self,
            request: impl tonic::IntoRequest<super::ListWorkersRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListWorkersResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/ListWorkers",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "ListWorkers"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_worker(
            &mut self,
            request: impl tonic::IntoRequest<super::GetWorkerRequest>,
        ) -> std::result::Result<tonic::Response<super::Worker>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/GetWorker",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "GetWorker"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_worker(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateWorkerRequest>,
        ) -> std::result::Result<tonic::Response<super::Worker>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/CreateWorker",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "CreateWorker"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn update_worker(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateWorkerRequest>,
        ) -> std::result::Result<tonic::Response<super::Worker>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/UpdateWorker",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "UpdateWorker"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn delete_worker(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteWorkerRequest>,
        ) -> std::result::Result<tonic::Response<super::Worker>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/DeleteWorker",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "DeleteWorker"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn drain_worker(
            &mut self,
            request: impl tonic::IntoRequest<super::DrainWorkerRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DrainWorkerResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/DrainWorker",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "DrainWorker"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn mark_worker_available(
            &mut self,
            request: impl tonic::IntoRequest<super::MarkWorkerAvailableRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MarkWorkerAvailableResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/MarkWorkerAvailable",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("substrate.ateapi.v1.Control", "MarkWorkerAvailable"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn mark_worker_unavailable(
            &mut self,
            request: impl tonic::IntoRequest<super::MarkWorkerUnavailableRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MarkWorkerUnavailableResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/MarkWorkerUnavailable",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "MarkWorkerUnavailable",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_worker_actor_assignments(
            &mut self,
            request: impl tonic::IntoRequest<super::ListWorkerActorAssignmentsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListWorkerActorAssignmentsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/ListWorkerActorAssignments",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "ListWorkerActorAssignments",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_actors(
            &mut self,
            request: impl tonic::IntoRequest<super::ListActorsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListActorsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/ListActors",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "ListActors"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_atespace(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateAtespaceRequest>,
        ) -> std::result::Result<tonic::Response<super::Atespace>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/CreateAtespace",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("substrate.ateapi.v1.Control", "CreateAtespace"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_atespace(
            &mut self,
            request: impl tonic::IntoRequest<super::GetAtespaceRequest>,
        ) -> std::result::Result<tonic::Response<super::Atespace>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/GetAtespace",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "GetAtespace"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_atespaces(
            &mut self,
            request: impl tonic::IntoRequest<super::ListAtespacesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListAtespacesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/ListAtespaces",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("substrate.ateapi.v1.Control", "ListAtespaces"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn delete_atespace(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteAtespaceRequest>,
        ) -> std::result::Result<tonic::Response<super::Atespace>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/DeleteAtespace",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("substrate.ateapi.v1.Control", "DeleteAtespace"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_actor_template(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateActorTemplateRequest>,
        ) -> std::result::Result<tonic::Response<super::ActorTemplate>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/CreateActorTemplate",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("substrate.ateapi.v1.Control", "CreateActorTemplate"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_actor_template(
            &mut self,
            request: impl tonic::IntoRequest<super::GetActorTemplateRequest>,
        ) -> std::result::Result<tonic::Response<super::ActorTemplate>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/GetActorTemplate",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("substrate.ateapi.v1.Control", "GetActorTemplate"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_actor_templates(
            &mut self,
            request: impl tonic::IntoRequest<super::ListActorTemplatesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListActorTemplatesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/ListActorTemplates",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("substrate.ateapi.v1.Control", "ListActorTemplates"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn delete_actor_template(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteActorTemplateRequest>,
        ) -> std::result::Result<tonic::Response<super::ActorTemplate>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/DeleteActorTemplate",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("substrate.ateapi.v1.Control", "DeleteActorTemplate"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_global_access_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::GetGlobalAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/GetGlobalAccessPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "GetGlobalAccessPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_global_access_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateGlobalAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/CreateGlobalAccessPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "CreateGlobalAccessPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn update_global_access_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateGlobalAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/UpdateGlobalAccessPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "UpdateGlobalAccessPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_atespace_access_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::GetAtespaceAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/GetAtespaceAccessPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "GetAtespaceAccessPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_atespace_access_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateAtespaceAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/CreateAtespaceAccessPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "CreateAtespaceAccessPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn update_atespace_access_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateAtespaceAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/UpdateAtespaceAccessPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "UpdateAtespaceAccessPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn delete_atespace_access_policy(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteAtespaceAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.Control/DeleteAtespaceAccessPolicy",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.Control",
                        "DeleteAtespaceAccessPolicy",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod control_server {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with ControlServer.
    #[async_trait]
    pub trait Control: std::marker::Send + std::marker::Sync + 'static {
        async fn get_actor(
            &self,
            request: tonic::Request<super::GetActorRequest>,
        ) -> std::result::Result<tonic::Response<super::Actor>, tonic::Status>;
        async fn create_actor(
            &self,
            request: tonic::Request<super::CreateActorRequest>,
        ) -> std::result::Result<tonic::Response<super::Actor>, tonic::Status>;
        async fn update_actor(
            &self,
            request: tonic::Request<super::UpdateActorRequest>,
        ) -> std::result::Result<tonic::Response<super::Actor>, tonic::Status>;
        async fn suspend_actor(
            &self,
            request: tonic::Request<super::SuspendActorRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SuspendActorResponse>,
            tonic::Status,
        >;
        async fn resume_actor(
            &self,
            request: tonic::Request<super::ResumeActorRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ResumeActorResponse>,
            tonic::Status,
        >;
        async fn revert_actor(
            &self,
            request: tonic::Request<super::RevertActorRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RevertActorResponse>,
            tonic::Status,
        >;
        async fn delete_actor(
            &self,
            request: tonic::Request<super::DeleteActorRequest>,
        ) -> std::result::Result<tonic::Response<super::Actor>, tonic::Status>;
        async fn get_actor_egress_policy(
            &self,
            request: tonic::Request<super::GetActorEgressPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::EgressPolicy>, tonic::Status>;
        async fn create_actor_egress_policy(
            &self,
            request: tonic::Request<super::CreateActorEgressPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::EgressPolicy>, tonic::Status>;
        async fn update_actor_egress_policy(
            &self,
            request: tonic::Request<super::UpdateActorEgressPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::EgressPolicy>, tonic::Status>;
        async fn delete_actor_egress_policy(
            &self,
            request: tonic::Request<super::DeleteActorEgressPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::EgressPolicy>, tonic::Status>;
        async fn mint_actor_jwt(
            &self,
            request: tonic::Request<super::MintActorJwtRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MintActorJwtResponse>,
            tonic::Status,
        >;
        async fn create_tag(
            &self,
            request: tonic::Request<super::CreateTagRequest>,
        ) -> std::result::Result<tonic::Response<super::Tag>, tonic::Status>;
        async fn get_tag(
            &self,
            request: tonic::Request<super::GetTagRequest>,
        ) -> std::result::Result<tonic::Response<super::Tag>, tonic::Status>;
        async fn list_tags(
            &self,
            request: tonic::Request<super::ListTagsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListTagsResponse>,
            tonic::Status,
        >;
        async fn update_tag(
            &self,
            request: tonic::Request<super::UpdateTagRequest>,
        ) -> std::result::Result<tonic::Response<super::Tag>, tonic::Status>;
        async fn delete_tag(
            &self,
            request: tonic::Request<super::DeleteTagRequest>,
        ) -> std::result::Result<tonic::Response<super::Tag>, tonic::Status>;
        async fn list_workers(
            &self,
            request: tonic::Request<super::ListWorkersRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListWorkersResponse>,
            tonic::Status,
        >;
        async fn get_worker(
            &self,
            request: tonic::Request<super::GetWorkerRequest>,
        ) -> std::result::Result<tonic::Response<super::Worker>, tonic::Status>;
        async fn create_worker(
            &self,
            request: tonic::Request<super::CreateWorkerRequest>,
        ) -> std::result::Result<tonic::Response<super::Worker>, tonic::Status>;
        async fn update_worker(
            &self,
            request: tonic::Request<super::UpdateWorkerRequest>,
        ) -> std::result::Result<tonic::Response<super::Worker>, tonic::Status>;
        async fn delete_worker(
            &self,
            request: tonic::Request<super::DeleteWorkerRequest>,
        ) -> std::result::Result<tonic::Response<super::Worker>, tonic::Status>;
        async fn drain_worker(
            &self,
            request: tonic::Request<super::DrainWorkerRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DrainWorkerResponse>,
            tonic::Status,
        >;
        async fn mark_worker_available(
            &self,
            request: tonic::Request<super::MarkWorkerAvailableRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MarkWorkerAvailableResponse>,
            tonic::Status,
        >;
        async fn mark_worker_unavailable(
            &self,
            request: tonic::Request<super::MarkWorkerUnavailableRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MarkWorkerUnavailableResponse>,
            tonic::Status,
        >;
        async fn list_worker_actor_assignments(
            &self,
            request: tonic::Request<super::ListWorkerActorAssignmentsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListWorkerActorAssignmentsResponse>,
            tonic::Status,
        >;
        async fn list_actors(
            &self,
            request: tonic::Request<super::ListActorsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListActorsResponse>,
            tonic::Status,
        >;
        async fn create_atespace(
            &self,
            request: tonic::Request<super::CreateAtespaceRequest>,
        ) -> std::result::Result<tonic::Response<super::Atespace>, tonic::Status>;
        async fn get_atespace(
            &self,
            request: tonic::Request<super::GetAtespaceRequest>,
        ) -> std::result::Result<tonic::Response<super::Atespace>, tonic::Status>;
        async fn list_atespaces(
            &self,
            request: tonic::Request<super::ListAtespacesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListAtespacesResponse>,
            tonic::Status,
        >;
        async fn delete_atespace(
            &self,
            request: tonic::Request<super::DeleteAtespaceRequest>,
        ) -> std::result::Result<tonic::Response<super::Atespace>, tonic::Status>;
        async fn create_actor_template(
            &self,
            request: tonic::Request<super::CreateActorTemplateRequest>,
        ) -> std::result::Result<tonic::Response<super::ActorTemplate>, tonic::Status>;
        async fn get_actor_template(
            &self,
            request: tonic::Request<super::GetActorTemplateRequest>,
        ) -> std::result::Result<tonic::Response<super::ActorTemplate>, tonic::Status>;
        async fn list_actor_templates(
            &self,
            request: tonic::Request<super::ListActorTemplatesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListActorTemplatesResponse>,
            tonic::Status,
        >;
        async fn delete_actor_template(
            &self,
            request: tonic::Request<super::DeleteActorTemplateRequest>,
        ) -> std::result::Result<tonic::Response<super::ActorTemplate>, tonic::Status>;
        async fn get_global_access_policy(
            &self,
            request: tonic::Request<super::GetGlobalAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status>;
        async fn create_global_access_policy(
            &self,
            request: tonic::Request<super::CreateGlobalAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status>;
        async fn update_global_access_policy(
            &self,
            request: tonic::Request<super::UpdateGlobalAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status>;
        async fn get_atespace_access_policy(
            &self,
            request: tonic::Request<super::GetAtespaceAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status>;
        async fn create_atespace_access_policy(
            &self,
            request: tonic::Request<super::CreateAtespaceAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status>;
        async fn update_atespace_access_policy(
            &self,
            request: tonic::Request<super::UpdateAtespaceAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status>;
        async fn delete_atespace_access_policy(
            &self,
            request: tonic::Request<super::DeleteAtespaceAccessPolicyRequest>,
        ) -> std::result::Result<tonic::Response<super::AccessPolicy>, tonic::Status>;
    }
    #[derive(Debug)]
    pub struct ControlServer<T> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    impl<T> ControlServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for ControlServer<T>
    where
        T: Control,
        B: Body + std::marker::Send + 'static,
        B::Error: Into<StdError> + std::marker::Send + 'static,
    {
        type Response = http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            match req.uri().path() {
                "/substrate.ateapi.v1.Control/GetActor" => {
                    #[allow(non_camel_case_types)]
                    struct GetActorSvc<T: Control>(pub Arc<T>);
                    impl<T: Control> tonic::server::UnaryService<super::GetActorRequest>
                    for GetActorSvc<T> {
                        type Response = super::Actor;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetActorRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::get_actor(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetActorSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/CreateActor" => {
                    #[allow(non_camel_case_types)]
                    struct CreateActorSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::CreateActorRequest>
                    for CreateActorSvc<T> {
                        type Response = super::Actor;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CreateActorRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::create_actor(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateActorSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/UpdateActor" => {
                    #[allow(non_camel_case_types)]
                    struct UpdateActorSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::UpdateActorRequest>
                    for UpdateActorSvc<T> {
                        type Response = super::Actor;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::UpdateActorRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::update_actor(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = UpdateActorSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/SuspendActor" => {
                    #[allow(non_camel_case_types)]
                    struct SuspendActorSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::SuspendActorRequest>
                    for SuspendActorSvc<T> {
                        type Response = super::SuspendActorResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::SuspendActorRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::suspend_actor(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = SuspendActorSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/ResumeActor" => {
                    #[allow(non_camel_case_types)]
                    struct ResumeActorSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::ResumeActorRequest>
                    for ResumeActorSvc<T> {
                        type Response = super::ResumeActorResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ResumeActorRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::resume_actor(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ResumeActorSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/RevertActor" => {
                    #[allow(non_camel_case_types)]
                    struct RevertActorSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::RevertActorRequest>
                    for RevertActorSvc<T> {
                        type Response = super::RevertActorResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RevertActorRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::revert_actor(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = RevertActorSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/DeleteActor" => {
                    #[allow(non_camel_case_types)]
                    struct DeleteActorSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::DeleteActorRequest>
                    for DeleteActorSvc<T> {
                        type Response = super::Actor;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::DeleteActorRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::delete_actor(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = DeleteActorSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/GetActorEgressPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct GetActorEgressPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::GetActorEgressPolicyRequest>
                    for GetActorEgressPolicySvc<T> {
                        type Response = super::EgressPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetActorEgressPolicyRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::get_actor_egress_policy(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetActorEgressPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/CreateActorEgressPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct CreateActorEgressPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::CreateActorEgressPolicyRequest>
                    for CreateActorEgressPolicySvc<T> {
                        type Response = super::EgressPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::CreateActorEgressPolicyRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::create_actor_egress_policy(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateActorEgressPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/UpdateActorEgressPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct UpdateActorEgressPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::UpdateActorEgressPolicyRequest>
                    for UpdateActorEgressPolicySvc<T> {
                        type Response = super::EgressPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::UpdateActorEgressPolicyRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::update_actor_egress_policy(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = UpdateActorEgressPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/DeleteActorEgressPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct DeleteActorEgressPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::DeleteActorEgressPolicyRequest>
                    for DeleteActorEgressPolicySvc<T> {
                        type Response = super::EgressPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::DeleteActorEgressPolicyRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::delete_actor_egress_policy(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = DeleteActorEgressPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/MintActorJwt" => {
                    #[allow(non_camel_case_types)]
                    struct MintActorJwtSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::MintActorJwtRequest>
                    for MintActorJwtSvc<T> {
                        type Response = super::MintActorJwtResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::MintActorJwtRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::mint_actor_jwt(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = MintActorJwtSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/CreateTag" => {
                    #[allow(non_camel_case_types)]
                    struct CreateTagSvc<T: Control>(pub Arc<T>);
                    impl<T: Control> tonic::server::UnaryService<super::CreateTagRequest>
                    for CreateTagSvc<T> {
                        type Response = super::Tag;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CreateTagRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::create_tag(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateTagSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/GetTag" => {
                    #[allow(non_camel_case_types)]
                    struct GetTagSvc<T: Control>(pub Arc<T>);
                    impl<T: Control> tonic::server::UnaryService<super::GetTagRequest>
                    for GetTagSvc<T> {
                        type Response = super::Tag;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetTagRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::get_tag(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetTagSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/ListTags" => {
                    #[allow(non_camel_case_types)]
                    struct ListTagsSvc<T: Control>(pub Arc<T>);
                    impl<T: Control> tonic::server::UnaryService<super::ListTagsRequest>
                    for ListTagsSvc<T> {
                        type Response = super::ListTagsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListTagsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::list_tags(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListTagsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/UpdateTag" => {
                    #[allow(non_camel_case_types)]
                    struct UpdateTagSvc<T: Control>(pub Arc<T>);
                    impl<T: Control> tonic::server::UnaryService<super::UpdateTagRequest>
                    for UpdateTagSvc<T> {
                        type Response = super::Tag;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::UpdateTagRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::update_tag(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = UpdateTagSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/DeleteTag" => {
                    #[allow(non_camel_case_types)]
                    struct DeleteTagSvc<T: Control>(pub Arc<T>);
                    impl<T: Control> tonic::server::UnaryService<super::DeleteTagRequest>
                    for DeleteTagSvc<T> {
                        type Response = super::Tag;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::DeleteTagRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::delete_tag(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = DeleteTagSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/ListWorkers" => {
                    #[allow(non_camel_case_types)]
                    struct ListWorkersSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::ListWorkersRequest>
                    for ListWorkersSvc<T> {
                        type Response = super::ListWorkersResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListWorkersRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::list_workers(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListWorkersSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/GetWorker" => {
                    #[allow(non_camel_case_types)]
                    struct GetWorkerSvc<T: Control>(pub Arc<T>);
                    impl<T: Control> tonic::server::UnaryService<super::GetWorkerRequest>
                    for GetWorkerSvc<T> {
                        type Response = super::Worker;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetWorkerRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::get_worker(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetWorkerSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/CreateWorker" => {
                    #[allow(non_camel_case_types)]
                    struct CreateWorkerSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::CreateWorkerRequest>
                    for CreateWorkerSvc<T> {
                        type Response = super::Worker;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CreateWorkerRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::create_worker(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateWorkerSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/UpdateWorker" => {
                    #[allow(non_camel_case_types)]
                    struct UpdateWorkerSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::UpdateWorkerRequest>
                    for UpdateWorkerSvc<T> {
                        type Response = super::Worker;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::UpdateWorkerRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::update_worker(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = UpdateWorkerSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/DeleteWorker" => {
                    #[allow(non_camel_case_types)]
                    struct DeleteWorkerSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::DeleteWorkerRequest>
                    for DeleteWorkerSvc<T> {
                        type Response = super::Worker;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::DeleteWorkerRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::delete_worker(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = DeleteWorkerSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/DrainWorker" => {
                    #[allow(non_camel_case_types)]
                    struct DrainWorkerSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::DrainWorkerRequest>
                    for DrainWorkerSvc<T> {
                        type Response = super::DrainWorkerResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::DrainWorkerRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::drain_worker(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = DrainWorkerSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/MarkWorkerAvailable" => {
                    #[allow(non_camel_case_types)]
                    struct MarkWorkerAvailableSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::MarkWorkerAvailableRequest>
                    for MarkWorkerAvailableSvc<T> {
                        type Response = super::MarkWorkerAvailableResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::MarkWorkerAvailableRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::mark_worker_available(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = MarkWorkerAvailableSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/MarkWorkerUnavailable" => {
                    #[allow(non_camel_case_types)]
                    struct MarkWorkerUnavailableSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::MarkWorkerUnavailableRequest>
                    for MarkWorkerUnavailableSvc<T> {
                        type Response = super::MarkWorkerUnavailableResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::MarkWorkerUnavailableRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::mark_worker_unavailable(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = MarkWorkerUnavailableSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/ListWorkerActorAssignments" => {
                    #[allow(non_camel_case_types)]
                    struct ListWorkerActorAssignmentsSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<
                        super::ListWorkerActorAssignmentsRequest,
                    > for ListWorkerActorAssignmentsSvc<T> {
                        type Response = super::ListWorkerActorAssignmentsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::ListWorkerActorAssignmentsRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::list_worker_actor_assignments(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListWorkerActorAssignmentsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/ListActors" => {
                    #[allow(non_camel_case_types)]
                    struct ListActorsSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::ListActorsRequest>
                    for ListActorsSvc<T> {
                        type Response = super::ListActorsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListActorsRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::list_actors(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListActorsSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/CreateAtespace" => {
                    #[allow(non_camel_case_types)]
                    struct CreateAtespaceSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::CreateAtespaceRequest>
                    for CreateAtespaceSvc<T> {
                        type Response = super::Atespace;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CreateAtespaceRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::create_atespace(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateAtespaceSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/GetAtespace" => {
                    #[allow(non_camel_case_types)]
                    struct GetAtespaceSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::GetAtespaceRequest>
                    for GetAtespaceSvc<T> {
                        type Response = super::Atespace;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetAtespaceRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::get_atespace(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetAtespaceSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/ListAtespaces" => {
                    #[allow(non_camel_case_types)]
                    struct ListAtespacesSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::ListAtespacesRequest>
                    for ListAtespacesSvc<T> {
                        type Response = super::ListAtespacesResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListAtespacesRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::list_atespaces(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListAtespacesSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/DeleteAtespace" => {
                    #[allow(non_camel_case_types)]
                    struct DeleteAtespaceSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::DeleteAtespaceRequest>
                    for DeleteAtespaceSvc<T> {
                        type Response = super::Atespace;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::DeleteAtespaceRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::delete_atespace(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = DeleteAtespaceSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/CreateActorTemplate" => {
                    #[allow(non_camel_case_types)]
                    struct CreateActorTemplateSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::CreateActorTemplateRequest>
                    for CreateActorTemplateSvc<T> {
                        type Response = super::ActorTemplate;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CreateActorTemplateRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::create_actor_template(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateActorTemplateSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/GetActorTemplate" => {
                    #[allow(non_camel_case_types)]
                    struct GetActorTemplateSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::GetActorTemplateRequest>
                    for GetActorTemplateSvc<T> {
                        type Response = super::ActorTemplate;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetActorTemplateRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::get_actor_template(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetActorTemplateSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/ListActorTemplates" => {
                    #[allow(non_camel_case_types)]
                    struct ListActorTemplatesSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::ListActorTemplatesRequest>
                    for ListActorTemplatesSvc<T> {
                        type Response = super::ListActorTemplatesResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListActorTemplatesRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::list_actor_templates(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ListActorTemplatesSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/DeleteActorTemplate" => {
                    #[allow(non_camel_case_types)]
                    struct DeleteActorTemplateSvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::DeleteActorTemplateRequest>
                    for DeleteActorTemplateSvc<T> {
                        type Response = super::ActorTemplate;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::DeleteActorTemplateRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::delete_actor_template(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = DeleteActorTemplateSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/GetGlobalAccessPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct GetGlobalAccessPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::GetGlobalAccessPolicyRequest>
                    for GetGlobalAccessPolicySvc<T> {
                        type Response = super::AccessPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetGlobalAccessPolicyRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::get_global_access_policy(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetGlobalAccessPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/CreateGlobalAccessPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct CreateGlobalAccessPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::CreateGlobalAccessPolicyRequest>
                    for CreateGlobalAccessPolicySvc<T> {
                        type Response = super::AccessPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::CreateGlobalAccessPolicyRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::create_global_access_policy(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateGlobalAccessPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/UpdateGlobalAccessPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct UpdateGlobalAccessPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::UpdateGlobalAccessPolicyRequest>
                    for UpdateGlobalAccessPolicySvc<T> {
                        type Response = super::AccessPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::UpdateGlobalAccessPolicyRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::update_global_access_policy(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = UpdateGlobalAccessPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/GetAtespaceAccessPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct GetAtespaceAccessPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<super::GetAtespaceAccessPolicyRequest>
                    for GetAtespaceAccessPolicySvc<T> {
                        type Response = super::AccessPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::GetAtespaceAccessPolicyRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::get_atespace_access_policy(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = GetAtespaceAccessPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/CreateAtespaceAccessPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct CreateAtespaceAccessPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<
                        super::CreateAtespaceAccessPolicyRequest,
                    > for CreateAtespaceAccessPolicySvc<T> {
                        type Response = super::AccessPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::CreateAtespaceAccessPolicyRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::create_atespace_access_policy(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CreateAtespaceAccessPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/UpdateAtespaceAccessPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct UpdateAtespaceAccessPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<
                        super::UpdateAtespaceAccessPolicyRequest,
                    > for UpdateAtespaceAccessPolicySvc<T> {
                        type Response = super::AccessPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::UpdateAtespaceAccessPolicyRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::update_atespace_access_policy(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = UpdateAtespaceAccessPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.Control/DeleteAtespaceAccessPolicy" => {
                    #[allow(non_camel_case_types)]
                    struct DeleteAtespaceAccessPolicySvc<T: Control>(pub Arc<T>);
                    impl<
                        T: Control,
                    > tonic::server::UnaryService<
                        super::DeleteAtespaceAccessPolicyRequest,
                    > for DeleteAtespaceAccessPolicySvc<T> {
                        type Response = super::AccessPolicy;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::DeleteAtespaceAccessPolicyRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Control>::delete_atespace_access_policy(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = DeleteAtespaceAccessPolicySvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => {
                    Box::pin(async move {
                        let mut response = http::Response::new(
                            tonic::body::Body::default(),
                        );
                        let headers = response.headers_mut();
                        headers
                            .insert(
                                tonic::Status::GRPC_STATUS,
                                (tonic::Code::Unimplemented as i32).into(),
                            );
                        headers
                            .insert(
                                http::header::CONTENT_TYPE,
                                tonic::metadata::GRPC_CONTENT_TYPE,
                            );
                        Ok(response)
                    })
                }
            }
        }
    }
    impl<T> Clone for ControlServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    /// Generated gRPC service name
    pub const SERVICE_NAME: &str = "substrate.ateapi.v1.Control";
    impl<T> tonic::server::NamedService for ControlServer<T> {
        const NAME: &'static str = SERVICE_NAME;
    }
}
/// Generated client implementations.
pub mod worker_service_client {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct WorkerServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl WorkerServiceClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> WorkerServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::Body>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + std::marker::Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + std::marker::Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> WorkerServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::Body>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::Body>,
            >>::Error: Into<StdError> + std::marker::Send + std::marker::Sync,
        {
            WorkerServiceClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        pub async fn acquire_worker_epoch(
            &mut self,
            request: impl tonic::IntoRequest<super::AcquireWorkerEpochRequest>,
        ) -> std::result::Result<
            tonic::Response<super::AcquireWorkerEpochResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.WorkerService/AcquireWorkerEpoch",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.WorkerService",
                        "AcquireWorkerEpoch",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn register_worker(
            &mut self,
            request: impl tonic::IntoRequest<super::RegisterWorkerRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RegisterWorkerResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.WorkerService/RegisterWorker",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.WorkerService",
                        "RegisterWorker",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn set_worker_sandbox_runtimes(
            &mut self,
            request: impl tonic::IntoRequest<super::SetWorkerSandboxRuntimesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SetWorkerSandboxRuntimesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.WorkerService/SetWorkerSandboxRuntimes",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.WorkerService",
                        "SetWorkerSandboxRuntimes",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn mint_worker_actor_certificate(
            &mut self,
            request: impl tonic::IntoRequest<super::MintWorkerActorCertificateRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MintWorkerActorCertificateResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.WorkerService/MintWorkerActorCertificate",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.WorkerService",
                        "MintWorkerActorCertificate",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn mint_worker_actor_jwt(
            &mut self,
            request: impl tonic::IntoRequest<super::MintWorkerActorJwtRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MintWorkerActorJwtResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.WorkerService/MintWorkerActorJwt",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.WorkerService",
                        "MintWorkerActorJwt",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn request_actor_suspend(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestActorSuspendRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RequestActorSuspendResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.WorkerService/RequestActorSuspend",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.WorkerService",
                        "RequestActorSuspend",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn report_actor_snapshot_state(
            &mut self,
            request: impl tonic::IntoRequest<super::ReportActorSnapshotStateRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ReportActorSnapshotStateResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.WorkerService/ReportActorSnapshotState",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.WorkerService",
                        "ReportActorSnapshotState",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn report_actor_crashed(
            &mut self,
            request: impl tonic::IntoRequest<super::ReportActorCrashedRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ReportActorCrashedResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/substrate.ateapi.v1.WorkerService/ReportActorCrashed",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "substrate.ateapi.v1.WorkerService",
                        "ReportActorCrashed",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod worker_service_server {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with WorkerServiceServer.
    #[async_trait]
    pub trait WorkerService: std::marker::Send + std::marker::Sync + 'static {
        async fn acquire_worker_epoch(
            &self,
            request: tonic::Request<super::AcquireWorkerEpochRequest>,
        ) -> std::result::Result<
            tonic::Response<super::AcquireWorkerEpochResponse>,
            tonic::Status,
        >;
        async fn register_worker(
            &self,
            request: tonic::Request<super::RegisterWorkerRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RegisterWorkerResponse>,
            tonic::Status,
        >;
        async fn set_worker_sandbox_runtimes(
            &self,
            request: tonic::Request<super::SetWorkerSandboxRuntimesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SetWorkerSandboxRuntimesResponse>,
            tonic::Status,
        >;
        async fn mint_worker_actor_certificate(
            &self,
            request: tonic::Request<super::MintWorkerActorCertificateRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MintWorkerActorCertificateResponse>,
            tonic::Status,
        >;
        async fn mint_worker_actor_jwt(
            &self,
            request: tonic::Request<super::MintWorkerActorJwtRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MintWorkerActorJwtResponse>,
            tonic::Status,
        >;
        async fn request_actor_suspend(
            &self,
            request: tonic::Request<super::RequestActorSuspendRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RequestActorSuspendResponse>,
            tonic::Status,
        >;
        async fn report_actor_snapshot_state(
            &self,
            request: tonic::Request<super::ReportActorSnapshotStateRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ReportActorSnapshotStateResponse>,
            tonic::Status,
        >;
        async fn report_actor_crashed(
            &self,
            request: tonic::Request<super::ReportActorCrashedRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ReportActorCrashedResponse>,
            tonic::Status,
        >;
    }
    #[derive(Debug)]
    pub struct WorkerServiceServer<T> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    impl<T> WorkerServiceServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for WorkerServiceServer<T>
    where
        T: WorkerService,
        B: Body + std::marker::Send + 'static,
        B::Error: Into<StdError> + std::marker::Send + 'static,
    {
        type Response = http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            match req.uri().path() {
                "/substrate.ateapi.v1.WorkerService/AcquireWorkerEpoch" => {
                    #[allow(non_camel_case_types)]
                    struct AcquireWorkerEpochSvc<T: WorkerService>(pub Arc<T>);
                    impl<
                        T: WorkerService,
                    > tonic::server::UnaryService<super::AcquireWorkerEpochRequest>
                    for AcquireWorkerEpochSvc<T> {
                        type Response = super::AcquireWorkerEpochResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::AcquireWorkerEpochRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WorkerService>::acquire_worker_epoch(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = AcquireWorkerEpochSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.WorkerService/RegisterWorker" => {
                    #[allow(non_camel_case_types)]
                    struct RegisterWorkerSvc<T: WorkerService>(pub Arc<T>);
                    impl<
                        T: WorkerService,
                    > tonic::server::UnaryService<super::RegisterWorkerRequest>
                    for RegisterWorkerSvc<T> {
                        type Response = super::RegisterWorkerResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RegisterWorkerRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WorkerService>::register_worker(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = RegisterWorkerSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.WorkerService/SetWorkerSandboxRuntimes" => {
                    #[allow(non_camel_case_types)]
                    struct SetWorkerSandboxRuntimesSvc<T: WorkerService>(pub Arc<T>);
                    impl<
                        T: WorkerService,
                    > tonic::server::UnaryService<super::SetWorkerSandboxRuntimesRequest>
                    for SetWorkerSandboxRuntimesSvc<T> {
                        type Response = super::SetWorkerSandboxRuntimesResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::SetWorkerSandboxRuntimesRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WorkerService>::set_worker_sandbox_runtimes(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = SetWorkerSandboxRuntimesSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.WorkerService/MintWorkerActorCertificate" => {
                    #[allow(non_camel_case_types)]
                    struct MintWorkerActorCertificateSvc<T: WorkerService>(pub Arc<T>);
                    impl<
                        T: WorkerService,
                    > tonic::server::UnaryService<
                        super::MintWorkerActorCertificateRequest,
                    > for MintWorkerActorCertificateSvc<T> {
                        type Response = super::MintWorkerActorCertificateResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::MintWorkerActorCertificateRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WorkerService>::mint_worker_actor_certificate(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = MintWorkerActorCertificateSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.WorkerService/MintWorkerActorJwt" => {
                    #[allow(non_camel_case_types)]
                    struct MintWorkerActorJwtSvc<T: WorkerService>(pub Arc<T>);
                    impl<
                        T: WorkerService,
                    > tonic::server::UnaryService<super::MintWorkerActorJwtRequest>
                    for MintWorkerActorJwtSvc<T> {
                        type Response = super::MintWorkerActorJwtResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::MintWorkerActorJwtRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WorkerService>::mint_worker_actor_jwt(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = MintWorkerActorJwtSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.WorkerService/RequestActorSuspend" => {
                    #[allow(non_camel_case_types)]
                    struct RequestActorSuspendSvc<T: WorkerService>(pub Arc<T>);
                    impl<
                        T: WorkerService,
                    > tonic::server::UnaryService<super::RequestActorSuspendRequest>
                    for RequestActorSuspendSvc<T> {
                        type Response = super::RequestActorSuspendResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::RequestActorSuspendRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WorkerService>::request_actor_suspend(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = RequestActorSuspendSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.WorkerService/ReportActorSnapshotState" => {
                    #[allow(non_camel_case_types)]
                    struct ReportActorSnapshotStateSvc<T: WorkerService>(pub Arc<T>);
                    impl<
                        T: WorkerService,
                    > tonic::server::UnaryService<super::ReportActorSnapshotStateRequest>
                    for ReportActorSnapshotStateSvc<T> {
                        type Response = super::ReportActorSnapshotStateResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::ReportActorSnapshotStateRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WorkerService>::report_actor_snapshot_state(
                                        &inner,
                                        request,
                                    )
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ReportActorSnapshotStateSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/substrate.ateapi.v1.WorkerService/ReportActorCrashed" => {
                    #[allow(non_camel_case_types)]
                    struct ReportActorCrashedSvc<T: WorkerService>(pub Arc<T>);
                    impl<
                        T: WorkerService,
                    > tonic::server::UnaryService<super::ReportActorCrashedRequest>
                    for ReportActorCrashedSvc<T> {
                        type Response = super::ReportActorCrashedResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ReportActorCrashedRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as WorkerService>::report_actor_crashed(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ReportActorCrashedSvc(inner);
                        let codec = tonic_prost::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => {
                    Box::pin(async move {
                        let mut response = http::Response::new(
                            tonic::body::Body::default(),
                        );
                        let headers = response.headers_mut();
                        headers
                            .insert(
                                tonic::Status::GRPC_STATUS,
                                (tonic::Code::Unimplemented as i32).into(),
                            );
                        headers
                            .insert(
                                http::header::CONTENT_TYPE,
                                tonic::metadata::GRPC_CONTENT_TYPE,
                            );
                        Ok(response)
                    })
                }
            }
        }
    }
    impl<T> Clone for WorkerServiceServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    /// Generated gRPC service name
    pub const SERVICE_NAME: &str = "substrate.ateapi.v1.WorkerService";
    impl<T> tonic::server::NamedService for WorkerServiceServer<T> {
        const NAME: &'static str = SERVICE_NAME;
    }
}
