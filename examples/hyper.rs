extern crate futures;
extern crate hyper;
extern crate mpart_async;
extern crate tokio;

use std::str;

use futures::future;
use futures::{Future, Stream};
use hyper::header::CONTENT_TYPE;
use hyper::service::service_fn;
use hyper::{Body, Request, Response, Server};
use mpart_async::MultipartRequest;
use tokio::runtime::current_thread::Runtime;

type BoxFut = Box<Future<Item = Response<Body>, Error = hyper::Error> + Send>;

fn handler(req: Request<Body>) -> BoxFut {
    println!("handle {:#?}", req);
    Box::new(req.into_body().concat2().and_then(|body| {
        println!("{}", str::from_utf8(&body).expect("print body"));
        future::ok(Response::new(Body::from("Ok")))
    }))
}

fn main() -> Result<(), String> {
    let mut rt = Runtime::new().expect("new rt");

    // Create and spawn http server
    let addr = ([127, 0, 0, 1], 3000).into();
    let server = Server::bind(&addr)
        .serve(|| service_fn(handler))
        .map_err(|e| eprintln!("server error: {}", e));

    rt.spawn(server);

    // Create request
    let mut mpart = MultipartRequest::default();
    mpart.add_field("foo", "bar");

    mpart.add_stream(
        "foofile",
        "foo.txt",
        "text/plain",
        Box::new(Body::from("Hello World").map(|c| c.into_bytes())),
    );

    let request = Request::post("http://localhost:3000")
        .header(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={}", mpart.get_boundary()),
        ).body(Body::wrap_stream(mpart))
        .map_err(|e| format!("{:?}", e))?;

    // Send request
    let client = hyper::Client::new();
    let fut = client.request(request).and_then(|response| {
            response.into_body().concat2().and_then(|body| {
            if let Ok(data) = str::from_utf8(&body) {
                println!("response: {}", data);
            }
            Ok(())
        })
    });
    rt.block_on(fut).expect("request failed");

    Ok(())
}
