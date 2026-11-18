pub mod proto {
    connectrpc::include_generated!();
}

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buffa_types::google::protobuf::Value;
use connectrpc::client::{ClientConfig, HttpClient};
use connectrpc::interceptor::{Interceptor, Next, UnaryRequest, UnaryResponse};
use connectrpc::{
    ConnectError, ErrorCode, RequestContext, Response, ServiceRequest, ServiceResult,
};
use tokio::net::TcpListener;

use proto::epm::v1::{
    GetLagRequest, GetLagResponse, ListDefinitionsRequest, ListDefinitionsResponse, Measure,
    ReadCellsRequest, ReadCellsResponse, SubmitEditRequest, SubmitEditResponse, Workspace,
    WorkspaceClient,
};

pub const WORKSPACE_HEADER: &str = "epm-workspace";
pub const EPOCH_HEADER: &str = "epm-epoch";

// Above 2^53, so a JSON number in a double would lose the last digit.
pub const FIRST_SEQUENCE: u64 = (1 << 53) + 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EditValue {
    Set(f64),
    Clear,
}

pub struct Assignee {
    failure: Option<ErrorCode>,
    acknowledgement_delay: Duration,
    accepted: Mutex<HashMap<String, (u64, EditValue)>>,
    observed_time_remaining: Mutex<Vec<Option<Duration>>>,
}

pub struct EpochCheck {
    pub workspace: String,
    pub epoch: u64,
}

fn edit_value(value: Option<&Value>) -> Result<EditValue, ConnectError> {
    match value {
        None => Err(ConnectError::invalid_argument("value is required")),
        Some(value) if value.is_null() => Ok(EditValue::Clear),
        Some(value) => value
            .as_number()
            .map(EditValue::Set)
            .ok_or_else(|| ConnectError::invalid_argument("value must be a number or null")),
    }
}

pub fn router(assignee: Arc<Assignee>, check: EpochCheck) -> axum::Router {
    let connect = connectrpc::Router::new()
        .add_service(assignee)
        .into_axum_service()
        .with_interceptor(check);
    axum::Router::new()
        .route("/health", axum::routing::get(|| async { "ok" }))
        .fallback_service(connect)
}

pub async fn spawn(router: axum::Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local address");
    tokio::spawn(async move { axum::serve(listener, router).await });
    address
}

pub fn assignee_client(
    address: SocketAddr,
    workspace: &str,
    epoch: u64,
) -> WorkspaceClient<HttpClient> {
    let config = ClientConfig::new(format!("http://{address}").parse().expect("uri"))
        .with_default_header(WORKSPACE_HEADER, workspace)
        .with_default_header(EPOCH_HEADER, epoch.to_string());
    WorkspaceClient::new(HttpClient::plaintext(), config)
}

impl Assignee {
    pub fn new(failure: Option<ErrorCode>, acknowledgement_delay: Duration) -> Assignee {
        Assignee {
            failure,
            acknowledgement_delay,
            accepted: Mutex::new(HashMap::new()),
            observed_time_remaining: Mutex::new(Vec::new()),
        }
    }

    pub fn accepted_edits(&self) -> usize {
        self.accepted.lock().expect("lock").len()
    }

    pub fn accepted_value(&self, request_id: &str) -> Option<EditValue> {
        let accepted = self.accepted.lock().expect("lock");
        accepted.get(request_id).map(|(_, value)| *value)
    }

    pub fn observed_time_remaining(&self) -> Vec<Option<Duration>> {
        self.observed_time_remaining.lock().expect("lock").clone()
    }

    fn fail_if_configured(&self) -> Result<(), ConnectError> {
        match self.failure {
            Some(code) => Err(ConnectError::new(code, "configured failure")),
            None => Ok(()),
        }
    }
}

#[expect(
    refining_impl_trait,
    reason = "a concrete message is easier to read than the impl Encodable return type, and the generated trait accepts it"
)]
impl Workspace for Assignee {
    async fn submit_edit(
        &self,
        context: RequestContext,
        request: ServiceRequest<'_, SubmitEditRequest>,
    ) -> ServiceResult<SubmitEditResponse> {
        self.fail_if_configured()?;
        self.observed_time_remaining
            .lock()
            .expect("lock")
            .push(context.time_remaining());
        let request = request.to_owned_message();
        let value = edit_value(request.value.as_option())?;
        let (sequence, new) = {
            let mut accepted = self.accepted.lock().expect("lock");
            let next = FIRST_SEQUENCE + accepted.len() as u64;
            match accepted.get(&request.request_id) {
                Some((sequence, _)) => (*sequence, false),
                None => {
                    accepted.insert(request.request_id, (next, value));
                    (next, true)
                }
            }
        };
        if new {
            tokio::time::sleep(self.acknowledgement_delay).await;
        }
        Response::ok(SubmitEditResponse {
            sequence,
            ..Default::default()
        })
    }

    async fn read_cells(
        &self,
        _context: RequestContext,
        request: ServiceRequest<'_, ReadCellsRequest>,
    ) -> ServiceResult<ReadCellsResponse> {
        self.fail_if_configured()?;
        let request = request.to_owned_message();
        let values = (0..request.cells.len())
            .map(|index| {
                if index % 2 == 0 {
                    Value::from(1.5)
                } else {
                    Value::null()
                }
            })
            .collect();
        Response::ok(ReadCellsResponse {
            recalculated: request.at_least,
            values,
            ..Default::default()
        })
    }

    async fn list_definitions(
        &self,
        _context: RequestContext,
        _request: ServiceRequest<'_, ListDefinitionsRequest>,
    ) -> ServiceResult<ListDefinitionsResponse> {
        self.fail_if_configured()?;
        Response::ok(ListDefinitionsResponse {
            measures: vec![Measure {
                ulid: "00000000000000000000000034".to_owned(),
                name: "Units".to_owned(),
                axes: vec!["00000000000000000000000001".to_owned()],
                kind: "input".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        })
    }

    async fn get_lag(
        &self,
        _context: RequestContext,
        _request: ServiceRequest<'_, GetLagRequest>,
    ) -> ServiceResult<GetLagResponse> {
        self.fail_if_configured()?;
        Response::ok(GetLagResponse {
            committed: Some(FIRST_SEQUENCE),
            recalculated: None,
            ..Default::default()
        })
    }
}

#[connectrpc::async_trait]
impl Interceptor for EpochCheck {
    async fn intercept_unary(
        &self,
        request: UnaryRequest,
        next: Next<'_>,
    ) -> Result<UnaryResponse, ConnectError> {
        let header = |name| {
            request
                .ctx
                .header(name)
                .and_then(|value| value.to_str().ok())
        };
        let workspace_matches = header(WORKSPACE_HEADER) == Some(self.workspace.as_str());
        let epoch_matches = header(EPOCH_HEADER) == Some(self.epoch.to_string().as_str());
        if !(workspace_matches && epoch_matches) {
            return Err(ConnectError::failed_precondition(format!(
                "this assignee serves workspace {} at epoch {}",
                self.workspace, self.epoch
            )));
        }
        next.run(request).await
    }
}
