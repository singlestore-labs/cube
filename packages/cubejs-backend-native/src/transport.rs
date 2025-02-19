use log::{debug, error, trace};
use neon::prelude::*;

use async_trait::async_trait;
use cubeclient::models::{V1Error, V1LoadRequestQuery, V1LoadResponse, V1MetaResponse};
use cubesql::compile::engine::df::scan::{MemberField, SchemaRef};
use cubesql::{
    di_service,
    sql::AuthContextRef,
    transport::{CubeStreamReceiver, LoadRequestMeta, MetaContext, TransportService},
    CubeError,
};
use serde_derive::Serialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::NativeAuthContext;
use crate::{
    auth::TransportRequest, channel::call_js_with_channel_as_callback,
    stream::call_js_with_stream_as_callback,
};

#[derive(Debug)]
pub struct NodeBridgeTransport {
    channel: Arc<Channel>,
    on_load: Arc<Root<JsFunction>>,
    on_meta: Arc<Root<JsFunction>>,
    on_load_stream: Arc<Root<JsFunction>>,
}

impl NodeBridgeTransport {
    pub fn new(
        channel: Channel,
        on_load: Root<JsFunction>,
        on_meta: Root<JsFunction>,
        on_load_stream: Root<JsFunction>,
    ) -> Self {
        Self {
            channel: Arc::new(channel),
            on_load: Arc::new(on_load),
            on_meta: Arc::new(on_meta),
            on_load_stream: Arc::new(on_load_stream),
        }
    }
}

#[derive(Debug, Serialize)]
struct SessionContext {
    user: Option<String>,
    superuser: bool,
}

#[derive(Debug, Serialize)]
struct LoadRequest {
    request: TransportRequest,
    query: V1LoadRequestQuery,
    session: SessionContext,
}

#[derive(Debug, Serialize)]
struct MetaRequest {
    request: TransportRequest,
    session: SessionContext,
}

#[async_trait]
impl TransportService for NodeBridgeTransport {
    async fn meta(&self, ctx: AuthContextRef) -> Result<Arc<MetaContext>, CubeError> {
        trace!("[transport] Meta ->");

        let native_auth = ctx
            .as_any()
            .downcast_ref::<NativeAuthContext>()
            .expect("Unable to cast AuthContext to NativeAuthContext");

        let request_id = Uuid::new_v4().to_string();
        let extra = serde_json::to_string(&MetaRequest {
            request: TransportRequest {
                id: format!("{}-span-1", request_id),
                meta: None,
            },
            session: SessionContext {
                user: native_auth.user.clone(),
                superuser: native_auth.superuser,
            },
        })?;
        let response = call_js_with_channel_as_callback::<V1MetaResponse>(
            self.channel.clone(),
            self.on_meta.clone(),
            Some(extra),
        )
        .await?;
        #[cfg(debug_assertions)]
        trace!("[transport] Meta <- {:?}", response);
        #[cfg(not(debug_assertions))]
        trace!("[transport] Meta <- <hidden>");

        Ok(Arc::new(MetaContext::new(
            response.cubes.unwrap_or_default(),
        )))
    }

    async fn load(
        &self,
        query: V1LoadRequestQuery,
        ctx: AuthContextRef,
        meta: LoadRequestMeta,
    ) -> Result<V1LoadResponse, CubeError> {
        trace!("[transport] Request ->");

        let native_auth = ctx
            .as_any()
            .downcast_ref::<NativeAuthContext>()
            .expect("Unable to cast AuthContext to NativeAuthContext");

        let request_id = Uuid::new_v4().to_string();
        let mut span_counter: u32 = 1;

        loop {
            let extra = serde_json::to_string(&LoadRequest {
                request: TransportRequest {
                    id: format!("{}-span-{}", request_id, span_counter),
                    meta: Some(meta.clone()),
                },
                query: query.clone(),
                session: SessionContext {
                    user: native_auth.user.clone(),
                    superuser: native_auth.superuser,
                },
            })?;

            let response: serde_json::Value = call_js_with_channel_as_callback(
                self.channel.clone(),
                self.on_load.clone(),
                Some(extra),
            )
            .await?;
            #[cfg(debug_assertions)]
            trace!("[transport] Request <- {:?}", response);
            #[cfg(not(debug_assertions))]
            trace!("[transport] Request <- <hidden>");

            let load_err = match serde_json::from_value::<V1LoadResponse>(response.clone()) {
                Ok(r) => {
                    return Ok(r);
                }
                Err(err) => err,
            };

            if let Ok(res) = serde_json::from_value::<V1Error>(response) {
                if res.error.to_lowercase() == *"continue wait" {
                    debug!(
                        "[transport] load - retrying request (continue wait) requestId: {}, span: {}",
                        request_id, span_counter
                    );

                    span_counter += 1;

                    continue;
                } else {
                    error!(
                        "[transport] load - strange response, success which contains error: {:?}",
                        res
                    );

                    return Err(CubeError::internal(res.error));
                }
            };

            return Err(CubeError::user(load_err.to_string()));
        }
    }

    async fn load_stream(
        &self,
        query: V1LoadRequestQuery,
        ctx: AuthContextRef,
        meta: LoadRequestMeta,
        schema: SchemaRef,
        member_fields: Vec<MemberField>,
    ) -> Result<CubeStreamReceiver, CubeError> {
        trace!("[transport] Request ->");

        let request_id = Uuid::new_v4().to_string();
        let mut span_counter: u32 = 1;
        loop {
            let native_auth = ctx
                .as_any()
                .downcast_ref::<NativeAuthContext>()
                .expect("Unable to cast AuthContext to NativeAuthContext");

            let extra = serde_json::to_string(&LoadRequest {
                request: TransportRequest {
                    id: format!("{}-span-{}", request_id, span_counter),
                    meta: Some(meta.clone()),
                },
                query: query.clone(),
                session: SessionContext {
                    user: native_auth.user.clone(),
                    superuser: native_auth.superuser,
                },
            })?;

            let res = call_js_with_stream_as_callback(
                self.channel.clone(),
                self.on_load_stream.clone(),
                Some(extra),
                schema.clone(),
                member_fields.clone(),
            )
            .await;

            if let Err(e) = &res {
                if e.message.to_lowercase().contains("continue wait") {
                    span_counter += 1;
                    continue;
                }
            }

            break res;
        }
    }
}

di_service!(NodeBridgeTransport, [TransportService]);
