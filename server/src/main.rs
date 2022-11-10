use aws_sdk_dynamodb::{model::AttributeValue, types::SdkError};
use aws_smithy_http::body::SdkBody;
use axum::extract::Extension;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use http::StatusCode;
use lambda_http::Error;
use serde::Deserialize;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tower::Layer;
use tower_http::{compression::CompressionLayer, limit::RequestBodyLimitLayer};
use tower_service::Service;
use uuid::Uuid;

#[allow(unused_imports)]
use tracing::{debug, error, info, trace, warn};

#[cfg(debug_assertions)]
const SEED: &str = include_str!("test.json");

#[derive(Clone, Debug)]
enum Backend {
    Dynamo(aws_sdk_dynamodb::Client),
    Local(Arc<Mutex<Local>>),
}

#[derive(Clone, Debug, Default)]
struct Local {
    events: HashMap<Uuid, String>,
    questions: HashMap<Uuid, HashMap<&'static str, AttributeValue>>,
    questions_by_eid: HashMap<Uuid, Vec<Uuid>>,
}

mod ask;
mod list;
mod new;
mod question;
mod toggle;
mod vote;

async fn check_secret(dynamo: &Backend, eid: &Uuid, secret: &str) -> Result<(), StatusCode> {
    match dynamo {
        Backend::Dynamo(dynamo) => {
            match dynamo
                .get_item()
                .table_name("events")
                .key("id", AttributeValue::S(eid.to_string()))
                .projection_expression("secret")
                .send()
                .await
            {
                Ok(v) => {
                    if v.item()
                        .and_then(|e| e.get("secret"))
                        .and_then(|s| s.as_s().ok())
                        .map_or(false, |s| s == secret)
                    {
                        Ok(())
                    } else {
                        warn!(%eid, secret, "attempted to access event with incorrect secret");
                        Err(StatusCode::FORBIDDEN)
                    }
                }
                Err(e) => {
                    error!(%eid, error = %e, "dynamodb event request for secret verificaton failed");
                    Err(http::StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Backend::Local(local) => {
            let mut local = local.lock().unwrap();
            let Local { events, .. } = &mut *local;
            if events[eid] == secret {
                Ok(())
            } else {
                Err(StatusCode::FORBIDDEN)
            }
        }
    }
}

fn mint_service_error<E>(e: E) -> SdkError<E> {
    SdkError::ServiceError {
        err: e,
        raw: aws_smithy_http::operation::Response::new(
            http::Response::builder().body(SdkBody::empty()).unwrap(),
        ),
    }
}

#[cfg(debug_assertions)]
#[derive(Deserialize)]
struct LiveAskQuestion {
    likes: usize,
    text: String,
    hidden: bool,
    answered: bool,
    #[serde(rename = "createTimeUnix")]
    created: usize,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        // .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .without_time()
        .init();

    let config = aws_config::load_from_env().await;

    let backend = if cfg!(debug_assertions) {
        let mut state = Local::default();
        let seed: Vec<LiveAskQuestion> = serde_json::from_str(SEED).unwrap();
        let seed_e = "00000000-0000-0000-0000-000000000000";
        let seed_e = Uuid::parse_str(seed_e).unwrap();
        state.events.insert(seed_e.clone(), String::from("secret"));
        state.questions_by_eid.insert(seed_e.clone(), Vec::new());
        let mut state = Backend::Local(Arc::new(Mutex::new(state)));
        let mut qs = Vec::new();
        for q in seed {
            let qid = uuid::Uuid::new_v4();
            state.ask(&seed_e, &qid, q.text).await.unwrap();
            qs.push((qid, q.created, q.likes, q.hidden, q.answered));
        }
        {
            let Backend::Local(ref mut state): Backend = state else {
                unreachable!();
            };
            let state = Arc::get_mut(state).unwrap();
            let state = Mutex::get_mut(state).unwrap();
            for (qid, created, votes, hidden, answered) in qs {
                let q = state.questions.get_mut(&qid).unwrap();
                q.insert("votes", AttributeValue::N(votes.to_string()));
                q.insert("answered", AttributeValue::Bool(answered));
                q.insert("hidden", AttributeValue::Bool(hidden));
                q.insert("when", AttributeValue::N(created.to_string()));
            }
        }
        state
    } else {
        Backend::Dynamo(aws_sdk_dynamodb::Client::new(&config))
    };

    let app = Router::new()
        .route("/event", post(new::new))
        .route("/event/:eid", get(list::list))
        .route("/event/:eid/:secret", get(list::list_all))
        .route(
            "/event/:eid/:secret/:qid/toggle/:property",
            post(toggle::toggle),
        )
        .route("/event/:eid", post(ask::ask))
        .route("/vote/:qid/:updown", post(vote::vote))
        .route("/question/:qid", get(question::question))
        .layer(Extension(backend))
        .layer(CompressionLayer::new().gzip(true).deflate(true))
        .layer(RequestBodyLimitLayer::new(512));

    if cfg!(debug_assertions) {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3000));
        Ok(axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await?)
    } else {
        // If we compile in release mode, use the Lambda Runtime
        // To run with AWS Lambda runtime, wrap in our `LambdaLayer`
        let app = tower::ServiceBuilder::new()
            .layer(LambdaLayer::default())
            .service(app);

        Ok(lambda_http::run(app).await?)
    }
}

#[derive(Default, Clone, Copy)]
pub struct LambdaLayer;

impl<S> Layer<S> for LambdaLayer {
    type Service = LambdaService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LambdaService { inner }
    }
}

pub struct LambdaService<S> {
    inner: S,
}

impl<S> Service<lambda_http::Request> for LambdaService<S>
where
    S: Service<axum::http::Request<axum::body::Body>>,
    S::Response: axum::response::IntoResponse + Send + 'static,
    S::Error: std::error::Error + Send + Sync + 'static,
    S::Future: Send + 'static,
{
    type Response = lambda_http::Response<lambda_http::Body>;
    type Error = lambda_http::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: lambda_http::Request) -> Self::Future {
        let (parts, body) = req.into_parts();
        let body = match body {
            lambda_http::Body::Empty => axum::body::Body::default(),
            lambda_http::Body::Text(t) => t.into(),
            lambda_http::Body::Binary(v) => v.into(),
        };

        let request = axum::http::Request::from_parts(parts, body);

        let fut = self.inner.call(request);
        let fut = async move {
            let resp = fut.await?;
            let (parts, body) = resp.into_response().into_parts();
            let bytes = hyper::body::to_bytes(body).await?;
            let bytes: &[u8] = &bytes;
            let resp: hyper::Response<lambda_http::Body> = match std::str::from_utf8(bytes) {
                Ok(s) => hyper::Response::from_parts(parts, s.into()),
                Err(_) => hyper::Response::from_parts(parts, bytes.into()),
            };
            Ok(resp)
        };

        Box::pin(fut)
    }
}