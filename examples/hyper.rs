extern crate futures;
extern crate hyper;
extern crate mpart_async;
extern crate tokio;

use std::str;

use futures::future;
use futures::{Future, IntoFuture, Stream};
use hyper::header::CONTENT_TYPE;
use hyper::service::service_fn;
use hyper::{Body, Request, Response, Server};
use tokio::codec::{BytesCodec, FramedRead};
use tokio::fs::File;
use tokio::runtime::Runtime;

use mpart_async::MultipartRequest;

type BoxFut = Box<Future<Item = Response<Body>, Error = hyper::Error> + Send>;

fn handler(req: Request<Body>) -> BoxFut {
    println!("Handle {:#?}", req);
    Box::new(req.into_body().concat2().and_then(|body| {
        println!("{}", str::from_utf8(&body).expect("print body"));
        future::ok(Response::new(Body::from("Ok")))
    }))
}

fn main() {
    // current_thread::Runtime can't be used because of the blocking file operations
    let mut rt = Runtime::new().expect("new rt");

    // Create and spawn http server
    let addr = ([127, 0, 0, 1], 3000).into();
    let server = Server::bind(&addr)
        .serve(|| service_fn(handler))
        .map_err(|e| eprintln!("server error: {}", e));

    rt.spawn(server);

    // Open `Cargo.toml` file and create a request
    let request = File::open("Cargo.toml")
        .map_err(|e| format!("{}", e))
        .and_then(|file| {
            // A Stream of BytesMut decoded from an AsyncRead
            let framed = FramedRead::new(file, BytesCodec::new());

            let mut mpart = MultipartRequest::default();
            mpart.add_field("foo", "bar");
            mpart.add_stream(
                "foofile",
                "Cargo.toml",
                "application/toml",
                framed.map(|b| b.freeze()),
            );
            Request::post("http://localhost:3000")
                .header(
                    CONTENT_TYPE,
                    format!("multipart/form-data; boundary={}", mpart.get_boundary()),
                ).body(Body::wrap_stream(mpart))
                .into_future()
                .map_err(|e| format!("{}", e))
        });

    // Send request
    let task = request.and_then(|request| {
        let client = hyper::Client::new();
        client
            .request(request)
            .map_err(|e| format!("{}", e))
            .and_then(|response| {
                response
                    .into_body()
                    .concat2()
                    .map_err(|e| format!("{}", e))
                    .and_then(|body| {
                        if let Ok(data) = str::from_utf8(&body) {
                            println!("Response: {}", data);
                        }
                        Ok(())
                    })
            })
    });

    rt.block_on(task).expect("request failed");
}
