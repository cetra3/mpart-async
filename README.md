# Rust Multipart Async

This crate allows the creation of client/server multipart streams for use with std futures.

## Quick Usage

With clients, you want to create a `MultipartRequest` & add in your fields & files.

### Hyper Client Example

Here is an [example](examples/hyper.rs) of how to use the client with hyper (`cargo run --example hyper`):

```rust
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
```

### Warp Server Example

Here is an [example](examples/warp.rs) of using it with the warp server (`cargo run --example warp`):

```rust
use warp::Filter;

use bytes::Buf;
use futures::stream::TryStreamExt;
use futures::Stream;
use mime::Mime;
use mpart_async::server::MultipartStream;
use std::convert::Infallible;

#[tokio::main]
async fn main() {
    // Match any request and return hello world!
    let routes = warp::any()
        .and(warp::header::<Mime>("content-type"))
        .and(warp::body::stream())
        .and_then(mpart);

    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;
}

async fn mpart(
    mime: Mime,
    body: impl Stream<Item = Result<impl Buf, warp::Error>> + Unpin,
) -> Result<impl warp::Reply, Infallible> {
    let boundary = mime.get_param("boundary").map(|v| v.to_string()).unwrap();

    let mut stream = MultipartStream::new(
        boundary,
        body.map_ok(|mut buf| buf.copy_to_bytes(buf.remaining())),
    );

    while let Ok(Some(mut field)) = stream.try_next().await {
        println!("Field received:{}", field.name().unwrap());
        if let Ok(filename) = field.filename() {
            println!("Field filename:{}", filename);
        }

        while let Ok(Some(bytes)) = field.try_next().await {
            println!("Bytes received:{}", bytes.len());
        }
    }

    Ok(format!("Thanks!\n"))
}
```
