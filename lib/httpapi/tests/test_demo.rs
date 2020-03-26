/*!
 * Test cases for the "demo" handlers.  These handlers exercise various
 * supported configurations of the HTTP handler interface.  We exercise them
 * here to make sure that even if these aren't used at a given point, they still
 * work.
 *
 * Note that the purpose is mainly to exercise the various possible function
 * signatures that can be used to implement handler functions.  We don't need to
 * exercises very many cases (or error cases) of each one because the handlers
 * themselves are not important, but we need to exercise enough to validate that
 * the generic JSON and query parsing handles error cases.
 *
 * TODO-hardening: add test cases that exceed limits (e.g., query string length,
 * JSON body length)
 */

use http::StatusCode;
use httpapi::test_util::make_request;
use httpapi::test_util::make_request_with_body;
use httpapi::test_util::read_json;
use httpapi::test_util::read_string;
use httpapi::test_util::ClientTestContext;
use httpapi::HttpError;
use httpapi::HttpRouteHandler;
use httpapi::HttpRouter;
use httpapi::HttpServer;
use httpapi::Json;
use httpapi::Query;
use httpapi::RequestContext;
use httpapi::CONTENT_TYPE_JSON;
use hyper::Body;
use hyper::Method;
use hyper::Response;
use serde::Deserialize;
use serde::Serialize;
use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::task::JoinHandle;

#[macro_use]
extern crate slog;
use slog::Drain;

struct DemoTestContext {
    client_testctx: ClientTestContext,
    server: HttpServer,
    server_task: JoinHandle<Result<(), hyper::error::Error>>,
}

impl DemoTestContext {
    async fn new() -> DemoTestContext {
        /*
         * The IP address to which we bind can be any local IP, but we use
         * 127.0.0.1 because we know it's present, it shouldn't expose this
         * server on any external network, and we don't have to go looking for
         * some other local IP (likely in a platform-specific way).  We specify
         * port 0 to request any available port.  This is important because we
         * may run multiple concurrent tests, so any fixed port could result in
         * spurious failures due to port conflicts.
         */
        let bind_address: SocketAddr = "127.0.0.1:0".parse().unwrap();

        /*
         * Set up a simple logger.
         */
        let log = {
            let plain = slog_term::PlainDecorator::new(std::io::stdout());
            let formatter = slog_term::FullFormat::new(plain).build().fuse();
            let drain = slog_async::Async::new(formatter).build().fuse();
            slog::Logger::root(drain, o!())
        };

        /*
         * Package up our test endpoints into a router.
         */
        let mut router = httpapi::HttpRouter::new();
        register_test_endpoints(&mut router);

        /*
         * Set up the server itself.
         */
        let mut server = httpapi::HttpServer::new(
            &bind_address,
            router,
            Box::new(0) as Box<dyn Any + Send + Sync + 'static>,
            &log,
        )
        .unwrap();
        let server_task = server.run();

        let server_addr = server.local_addr();
        let client_testctx = ClientTestContext::new(server_addr, &log);

        DemoTestContext {
            client_testctx,
            server,
            server_task,
        }
    }

    /*
     * TODO-cleanup: is there an async analog to Drop?
     */
    async fn teardown(self) {
        self.server.close();
        let join_result = self.server_task.await.unwrap();
        join_result.expect("server closed with an error");
    }
}

/*
 * The "demo1" handler consumes neither query nor JSON body parameters.  Here we
 * test that such handlers work.  There are no error cases for us to induce.
 */
#[tokio::test]
async fn test_demo1() {
    let testctx = DemoTestContext::new().await;
    let mut response = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo1",
        None as Option<()>,
        StatusCode::OK,
    )
    .await
    .expect("expected success");
    let body = read_string(&mut response).await;
    assert_eq!(body, "demo_handler_args_1\n");
    testctx.teardown().await;
}

/*
 * The "demo2query" handler consumes only query arguments.  Here we make sure
 * such handlers work and also exercise various error cases associated with bad
 * query string parsing.
 * TODO-hardening there are a lot more to check here, particularly around
 * encoded values.
 */
#[tokio::test]
async fn test_demo2query() {
    let testctx = DemoTestContext::new().await;

    /* Test case: optional field missing */
    let mut response = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo2query?test1=foo",
        None as Option<()>,
        StatusCode::OK,
    )
    .await
    .expect("expected success");
    let json: DemoJsonBody = read_json(&mut response).await;
    assert_eq!(json.test1, "foo");
    assert_eq!(json.test2, None);

    /* Test case: both fields specified */
    let mut response = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo2query?test1=foo&test2=10",
        None as Option<()>,
        StatusCode::OK,
    )
    .await
    .expect("expected success");
    let json: DemoJsonBody = read_json(&mut response).await;
    assert_eq!(json.test1, "foo");
    assert_eq!(json.test2, Some(10));

    /* Test case: required field missing */
    let error = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo2query",
        None as Option<()>,
        StatusCode::BAD_REQUEST,
    )
    .await
    .expect_err("expected failure");
    assert_eq!(
        error.message,
        "unable to parse query string: missing field `test1`"
    );

    /* Test case: typed field has bad value */
    let error = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo2query?test1=foo&test2=bar",
        None as Option<()>,
        StatusCode::BAD_REQUEST,
    )
    .await
    .expect_err("expected failure");
    assert_eq!(
        error.message,
        "unable to parse query string: invalid digit found in string"
    );

    /* Test case: duplicated field name */
    let error = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo2query?test1=foo&test1=bar",
        None as Option<()>,
        StatusCode::BAD_REQUEST,
    )
    .await
    .expect_err("expected failure");
    assert_eq!(
        error.message,
        "unable to parse query string: duplicate field `test1`"
    );

    testctx.teardown().await;
}

/*
 * The "demo2json" handler consumes only a JSON object.  Here we make sure such
 * handlers work and also exercise various error cases associated with bad JSON
 * handling.
 */
#[tokio::test]
async fn test_demo2json() {
    let testctx = DemoTestContext::new().await;

    /* Test case: optional field */
    let input = DemoJsonBody {
        test1: "bar".to_string(),
        test2: None,
    };
    let mut response = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo2json",
        Some(input),
        StatusCode::OK,
    )
    .await
    .expect("expected success");
    let json: DemoJsonBody = read_json(&mut response).await;
    assert_eq!(json.test1, "bar");
    assert_eq!(json.test2, None);

    /* Test case: both fields populated */
    let input = DemoJsonBody {
        test1: "bar".to_string(),
        test2: Some(15),
    };
    let mut response = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo2json",
        Some(input),
        StatusCode::OK,
    )
    .await
    .expect("expected success");
    let json: DemoJsonBody = read_json(&mut response).await;
    assert_eq!(json.test1, "bar");
    assert_eq!(json.test2, Some(15));

    /* Test case: no input specified */
    let error = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo2json",
        None as Option<()>,
        StatusCode::BAD_REQUEST,
    )
    .await
    .expect_err("expected failure");
    assert!(error.message.starts_with("unable to parse body JSON"));

    /* Test case: invalid JSON */
    let error = make_request_with_body(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo2json",
        "}".into(),
        StatusCode::BAD_REQUEST,
    )
    .await
    .expect_err("expected failure");
    assert!(error.message.starts_with("unable to parse body JSON"));

    /* Test case: bad type */
    let json_bad_type = "{ \"test1\": \"oops\", \"test2\": \"oops\" }";
    let error = make_request_with_body(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo2json",
        json_bad_type.into(),
        StatusCode::BAD_REQUEST,
    )
    .await
    .expect_err("expected failure");
    assert!(error.message.starts_with(
        "unable to parse body JSON: invalid type: string \"oops\", expected \
         u32"
    ));

    testctx.teardown().await;
}

/*
 * The "demo3" handler takes both query arguments and a JSON body.  This test
 * makes sure that both sets of parameters are received by the handler function
 * and at least one error case from each of those sources is exercised.  We
 * don't need exhaustively re-test the query and JSON error handling paths.
 */
#[tokio::test]
async fn test_demo3json() {
    let testctx = DemoTestContext::new().await;

    /* Test case: everything filled in. */
    let json_input = DemoJsonBody {
        test1: "bart".to_string(),
        test2: Some(0),
    };

    let mut response = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo3?test1=martin&test2=2",
        Some(json_input),
        StatusCode::OK,
    )
    .await
    .expect("expected success");
    let json: DemoJsonAndQuery = read_json(&mut response).await;
    assert_eq!(json.json.test1, "bart");
    assert_eq!(json.json.test2.unwrap(), 0);
    assert_eq!(json.query.test1, "martin");
    assert_eq!(json.query.test2.unwrap(), 2);

    /* Test case: error parsing query */
    let json_input = DemoJsonBody {
        test1: "bart".to_string(),
        test2: Some(0),
    };
    let error = make_request(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo3?test2=2",
        Some(json_input),
        StatusCode::BAD_REQUEST,
    )
    .await
    .expect_err("expected error");
    assert_eq!(
        error.message,
        "unable to parse query string: missing field `test1`"
    );

    /* Test case: error parsing body */
    let error = make_request_with_body(
        &testctx.client_testctx,
        Method::GET,
        "/testing/demo3?test1=martin&test2=2",
        "}".into(),
        StatusCode::BAD_REQUEST,
    )
    .await
    .expect_err("expected error");
    assert!(error.message.starts_with("unable to parse body JSON"));

    testctx.teardown().await;
}

/*
 * Demo handler functions
 * XXX cleanup
 */
pub fn register_test_endpoints(router: &mut HttpRouter) {
    router.insert(
        Method::GET,
        "/testing/demo1",
        HttpRouteHandler::new(demo_handler_args_1),
    );
    router.insert(
        Method::GET,
        "/testing/demo2query",
        HttpRouteHandler::new(demo_handler_args_2query),
    );
    router.insert(
        Method::GET,
        "/testing/demo2json",
        HttpRouteHandler::new(demo_handler_args_2json),
    );
    router.insert(
        Method::GET,
        "/testing/demo3",
        HttpRouteHandler::new(demo_handler_args_3),
    );
}

async fn demo_handler_args_1(
    _rqctx: Arc<RequestContext>,
) -> Result<Response<Body>, HttpError> {
    Ok(Response::builder()
        .header(http::header::CONTENT_TYPE, CONTENT_TYPE_JSON)
        .status(StatusCode::OK)
        .body("demo_handler_args_1\n".into())?)
}

#[derive(Serialize, Deserialize)]
pub struct DemoQueryArgs {
    pub test1: String,
    pub test2: Option<u32>,
}

async fn demo_handler_args_2query(
    _rqctx: Arc<RequestContext>,
    query: Query<DemoQueryArgs>,
) -> Result<Response<Body>, HttpError> {
    Ok(Response::builder()
        .header(http::header::CONTENT_TYPE, CONTENT_TYPE_JSON)
        .status(StatusCode::OK)
        .body(serde_json::to_string(&query.into_inner()).unwrap().into())?)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DemoJsonBody {
    pub test1: String,
    pub test2: Option<u32>,
}

async fn demo_handler_args_2json(
    _rqctx: Arc<RequestContext>,
    json: Json<DemoJsonBody>,
) -> Result<Response<Body>, HttpError> {
    Ok(Response::builder()
        .header(http::header::CONTENT_TYPE, CONTENT_TYPE_JSON)
        .status(StatusCode::OK)
        .body(serde_json::to_string(&json.into_inner()).unwrap().into())?)
}

#[derive(Deserialize, Serialize)]
pub struct DemoJsonAndQuery {
    pub query: DemoQueryArgs,
    pub json: DemoJsonBody,
}
async fn demo_handler_args_3(
    _rqctx: Arc<RequestContext>,
    query: Query<DemoQueryArgs>,
    json: Json<DemoJsonBody>,
) -> Result<Response<Body>, HttpError> {
    let combined = DemoJsonAndQuery {
        query: query.into_inner(),
        json: json.into_inner(),
    };
    Ok(Response::builder()
        .header(http::header::CONTENT_TYPE, CONTENT_TYPE_JSON)
        .status(StatusCode::OK)
        .body(serde_json::to_string(&combined).unwrap().into())?)
}