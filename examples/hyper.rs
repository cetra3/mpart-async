use hyper::{header::CONTENT_TYPE, Body, Client, Request};
use hyper::{service::make_service_fn, service::service_fn, Response, Server};
use mpart_async::client::MultipartRequest;

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    //Setup a mock server to accept connections.
    setup_server();

    let client = Client::new();

    let mut mpart = MultipartRequest::default();

    mpart.add_field("foo", "bar");
    mpart.add_file("test", "Cargo.toml");

    let request = Request::post("http://localhost:3000")
        .header(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={}", mpart.get_boundary()),
        )
        .body(Body::wrap_stream(mpart))?;

    client.request(request).await?;

    Ok(())
}

fn setup_server() {
    let addr = ([127, 0, 0, 1], 3000).into();
    let make_svc = make_service_fn(|_conn| async { Ok::<_, Error>(service_fn(mock)) });
    let server = Server::bind(&addr).serve(make_svc);

    tokio::spawn(server);
}

async fn mock(_: Request<Body>) -> Result<Response<Body>, Error> {
    Ok(Response::new(Body::from("")))
}
