// @generated
/// Generated client implementations.
pub mod a2a_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct A2aServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl A2aServiceClient<tonic::transport::Channel> {
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
    impl<T> A2aServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> A2aServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            A2aServiceClient::new(InterceptedService::new(inner, interceptor))
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
        pub async fn send_message(
            &mut self,
            request: impl tonic::IntoRequest<super::SendMessageRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SendMessageResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/SendMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("lf.a2a.v1.A2AService", "SendMessage"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn send_streaming_message(
            &mut self,
            request: impl tonic::IntoRequest<super::SendMessageRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::StreamResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/SendStreamingMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("lf.a2a.v1.A2AService", "SendStreamingMessage"));
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn get_task(
            &mut self,
            request: impl tonic::IntoRequest<super::GetTaskRequest>,
        ) -> std::result::Result<tonic::Response<super::Task>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/GetTask",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("lf.a2a.v1.A2AService", "GetTask"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_tasks(
            &mut self,
            request: impl tonic::IntoRequest<super::ListTasksRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListTasksResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/ListTasks",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("lf.a2a.v1.A2AService", "ListTasks"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn cancel_task(
            &mut self,
            request: impl tonic::IntoRequest<super::CancelTaskRequest>,
        ) -> std::result::Result<tonic::Response<super::Task>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/CancelTask",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("lf.a2a.v1.A2AService", "CancelTask"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn subscribe_to_task(
            &mut self,
            request: impl tonic::IntoRequest<super::SubscribeToTaskRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::StreamResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/SubscribeToTask",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("lf.a2a.v1.A2AService", "SubscribeToTask"));
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn create_task_push_notification_config(
            &mut self,
            request: impl tonic::IntoRequest<super::TaskPushNotificationConfig>,
        ) -> std::result::Result<
            tonic::Response<super::TaskPushNotificationConfig>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/CreateTaskPushNotificationConfig",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "lf.a2a.v1.A2AService",
                        "CreateTaskPushNotificationConfig",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_task_push_notification_config(
            &mut self,
            request: impl tonic::IntoRequest<super::GetTaskPushNotificationConfigRequest>,
        ) -> std::result::Result<
            tonic::Response<super::TaskPushNotificationConfig>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/GetTaskPushNotificationConfig",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "lf.a2a.v1.A2AService",
                        "GetTaskPushNotificationConfig",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_task_push_notification_configs(
            &mut self,
            request: impl tonic::IntoRequest<
                super::ListTaskPushNotificationConfigsRequest,
            >,
        ) -> std::result::Result<
            tonic::Response<super::ListTaskPushNotificationConfigsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/ListTaskPushNotificationConfigs",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "lf.a2a.v1.A2AService",
                        "ListTaskPushNotificationConfigs",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_extended_agent_card(
            &mut self,
            request: impl tonic::IntoRequest<super::GetExtendedAgentCardRequest>,
        ) -> std::result::Result<tonic::Response<super::AgentCard>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/GetExtendedAgentCard",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("lf.a2a.v1.A2AService", "GetExtendedAgentCard"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn delete_task_push_notification_config(
            &mut self,
            request: impl tonic::IntoRequest<
                super::DeleteTaskPushNotificationConfigRequest,
            >,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/lf.a2a.v1.A2AService/DeleteTaskPushNotificationConfig",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "lf.a2a.v1.A2AService",
                        "DeleteTaskPushNotificationConfig",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod a2a_service_server {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with A2aServiceServer.
    #[async_trait]
    pub trait A2aService: Send + Sync + 'static {
        async fn send_message(
            &self,
            request: tonic::Request<super::SendMessageRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SendMessageResponse>,
            tonic::Status,
        >;
        /// Server streaming response type for the SendStreamingMessage method.
        type SendStreamingMessageStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::StreamResponse, tonic::Status>,
            >
            + Send
            + 'static;
        async fn send_streaming_message(
            &self,
            request: tonic::Request<super::SendMessageRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::SendStreamingMessageStream>,
            tonic::Status,
        >;
        async fn get_task(
            &self,
            request: tonic::Request<super::GetTaskRequest>,
        ) -> std::result::Result<tonic::Response<super::Task>, tonic::Status>;
        async fn list_tasks(
            &self,
            request: tonic::Request<super::ListTasksRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListTasksResponse>,
            tonic::Status,
        >;
        async fn cancel_task(
            &self,
            request: tonic::Request<super::CancelTaskRequest>,
        ) -> std::result::Result<tonic::Response<super::Task>, tonic::Status>;
        /// Server streaming response type for the SubscribeToTask method.
        type SubscribeToTaskStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::StreamResponse, tonic::Status>,
            >
            + Send
            + 'static;
        async fn subscribe_to_task(
            &self,
            request: tonic::Request<super::SubscribeToTaskRequest>,
        ) -> std::result::Result<
            tonic::Response<Self::SubscribeToTaskStream>,
            tonic::Status,
        >;
        async fn create_task_push_notification_config(
            &self,
            request: tonic::Request<super::TaskPushNotificationConfig>,
        ) -> std::result::Result<
            tonic::Response<super::TaskPushNotificationConfig>,
            tonic::Status,
        >;
        async fn get_task_push_notification_config(
            &self,
            request: tonic::Request<super::GetTaskPushNotificationConfigRequest>,
        ) -> std::result::Result<
            tonic::Response<super::TaskPushNotificationConfig>,
            tonic::Status,
        >;
        async fn list_task_push_notification_configs(
            &self,
            request: tonic::Request<super::ListTaskPushNotificationConfigsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListTaskPushNotificationConfigsResponse>,
            tonic::Status,
        >;
        async fn get_extended_agent_card(
            &self,
            request: tonic::Request<super::GetExtendedAgentCardRequest>,
        ) -> std::result::Result<tonic::Response<super::AgentCard>, tonic::Status>;
        async fn delete_task_push_notification_config(
            &self,
            request: tonic::Request<super::DeleteTaskPushNotificationConfigRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status>;
    }
    #[derive(Debug)]
    pub struct A2aServiceServer<T: A2aService> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    impl<T: A2aService> A2aServiceServer<T> {
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
    impl<T, B> tonic::codegen::Service<http::Request<B>> for A2aServiceServer<T>
    where
        T: A2aService,
        B: Body + Send + 'static,
        B::Error: Into<StdError> + Send + 'static,
    {
        type Response = http::Response<tonic::body::BoxBody>;
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
                "/lf.a2a.v1.A2AService/SendMessage" => {
                    #[allow(non_camel_case_types)]
                    struct SendMessageSvc<T: A2aService>(pub Arc<T>);
                    impl<
                        T: A2aService,
                    > tonic::server::UnaryService<super::SendMessageRequest>
                    for SendMessageSvc<T> {
                        type Response = super::SendMessageResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::SendMessageRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::send_message(&inner, request).await
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
                        let method = SendMessageSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/lf.a2a.v1.A2AService/SendStreamingMessage" => {
                    #[allow(non_camel_case_types)]
                    struct SendStreamingMessageSvc<T: A2aService>(pub Arc<T>);
                    impl<
                        T: A2aService,
                    > tonic::server::ServerStreamingService<super::SendMessageRequest>
                    for SendStreamingMessageSvc<T> {
                        type Response = super::StreamResponse;
                        type ResponseStream = T::SendStreamingMessageStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::SendMessageRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::send_streaming_message(&inner, request)
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
                        let method = SendStreamingMessageSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/lf.a2a.v1.A2AService/GetTask" => {
                    #[allow(non_camel_case_types)]
                    struct GetTaskSvc<T: A2aService>(pub Arc<T>);
                    impl<
                        T: A2aService,
                    > tonic::server::UnaryService<super::GetTaskRequest>
                    for GetTaskSvc<T> {
                        type Response = super::Task;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetTaskRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::get_task(&inner, request).await
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
                        let method = GetTaskSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/lf.a2a.v1.A2AService/ListTasks" => {
                    #[allow(non_camel_case_types)]
                    struct ListTasksSvc<T: A2aService>(pub Arc<T>);
                    impl<
                        T: A2aService,
                    > tonic::server::UnaryService<super::ListTasksRequest>
                    for ListTasksSvc<T> {
                        type Response = super::ListTasksResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ListTasksRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::list_tasks(&inner, request).await
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
                        let method = ListTasksSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/lf.a2a.v1.A2AService/CancelTask" => {
                    #[allow(non_camel_case_types)]
                    struct CancelTaskSvc<T: A2aService>(pub Arc<T>);
                    impl<
                        T: A2aService,
                    > tonic::server::UnaryService<super::CancelTaskRequest>
                    for CancelTaskSvc<T> {
                        type Response = super::Task;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CancelTaskRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::cancel_task(&inner, request).await
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
                        let method = CancelTaskSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/lf.a2a.v1.A2AService/SubscribeToTask" => {
                    #[allow(non_camel_case_types)]
                    struct SubscribeToTaskSvc<T: A2aService>(pub Arc<T>);
                    impl<
                        T: A2aService,
                    > tonic::server::ServerStreamingService<
                        super::SubscribeToTaskRequest,
                    > for SubscribeToTaskSvc<T> {
                        type Response = super::StreamResponse;
                        type ResponseStream = T::SubscribeToTaskStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::SubscribeToTaskRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::subscribe_to_task(&inner, request).await
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
                        let method = SubscribeToTaskSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/lf.a2a.v1.A2AService/CreateTaskPushNotificationConfig" => {
                    #[allow(non_camel_case_types)]
                    struct CreateTaskPushNotificationConfigSvc<T: A2aService>(
                        pub Arc<T>,
                    );
                    impl<
                        T: A2aService,
                    > tonic::server::UnaryService<super::TaskPushNotificationConfig>
                    for CreateTaskPushNotificationConfigSvc<T> {
                        type Response = super::TaskPushNotificationConfig;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::TaskPushNotificationConfig>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::create_task_push_notification_config(
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
                        let method = CreateTaskPushNotificationConfigSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/lf.a2a.v1.A2AService/GetTaskPushNotificationConfig" => {
                    #[allow(non_camel_case_types)]
                    struct GetTaskPushNotificationConfigSvc<T: A2aService>(pub Arc<T>);
                    impl<
                        T: A2aService,
                    > tonic::server::UnaryService<
                        super::GetTaskPushNotificationConfigRequest,
                    > for GetTaskPushNotificationConfigSvc<T> {
                        type Response = super::TaskPushNotificationConfig;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::GetTaskPushNotificationConfigRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::get_task_push_notification_config(
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
                        let method = GetTaskPushNotificationConfigSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/lf.a2a.v1.A2AService/ListTaskPushNotificationConfigs" => {
                    #[allow(non_camel_case_types)]
                    struct ListTaskPushNotificationConfigsSvc<T: A2aService>(pub Arc<T>);
                    impl<
                        T: A2aService,
                    > tonic::server::UnaryService<
                        super::ListTaskPushNotificationConfigsRequest,
                    > for ListTaskPushNotificationConfigsSvc<T> {
                        type Response = super::ListTaskPushNotificationConfigsResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::ListTaskPushNotificationConfigsRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::list_task_push_notification_configs(
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
                        let method = ListTaskPushNotificationConfigsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/lf.a2a.v1.A2AService/GetExtendedAgentCard" => {
                    #[allow(non_camel_case_types)]
                    struct GetExtendedAgentCardSvc<T: A2aService>(pub Arc<T>);
                    impl<
                        T: A2aService,
                    > tonic::server::UnaryService<super::GetExtendedAgentCardRequest>
                    for GetExtendedAgentCardSvc<T> {
                        type Response = super::AgentCard;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::GetExtendedAgentCardRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::get_extended_agent_card(&inner, request)
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
                        let method = GetExtendedAgentCardSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                "/lf.a2a.v1.A2AService/DeleteTaskPushNotificationConfig" => {
                    #[allow(non_camel_case_types)]
                    struct DeleteTaskPushNotificationConfigSvc<T: A2aService>(
                        pub Arc<T>,
                    );
                    impl<
                        T: A2aService,
                    > tonic::server::UnaryService<
                        super::DeleteTaskPushNotificationConfigRequest,
                    > for DeleteTaskPushNotificationConfigSvc<T> {
                        type Response = ();
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<
                                super::DeleteTaskPushNotificationConfigRequest,
                            >,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as A2aService>::delete_task_push_notification_config(
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
                        let method = DeleteTaskPushNotificationConfigSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
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
                        Ok(
                            http::Response::builder()
                                .status(200)
                                .header("grpc-status", tonic::Code::Unimplemented as i32)
                                .header(
                                    http::header::CONTENT_TYPE,
                                    tonic::metadata::GRPC_CONTENT_TYPE,
                                )
                                .body(empty_body())
                                .unwrap(),
                        )
                    })
                }
            }
        }
    }
    impl<T: A2aService> Clone for A2aServiceServer<T> {
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
    impl<T: A2aService> tonic::server::NamedService for A2aServiceServer<T> {
        const NAME: &'static str = "lf.a2a.v1.A2AService";
    }
}
