# Rust Multipart Async

This crate allows the creation of client multipart streams for use with the futures-fs ecosystem.

## Quick Usage

### Actix Web > 0.6

```rust
/*
This method receives a pure binary body and converts it into a multipart request
*/
fn handle_put<S: 'static>(req: HttpRequest<S>, url: &str) -> Box<Future<Item = HttpResponse, Error = Error>> {

    let mut builder = client::ClientRequest::build();

    let mut mpart = MultipartRequest::default();

    let content_type = req.headers()
        .get(CONTENT_TYPE)
        .and_then(|val| val.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let file_name = "example_file.bin";

    mpart.add_stream(
        "content",
        file_name,
        &content_type,
        req.map_err(|err| err_msg(err)),
    );

    mpart.add_field("name", file_name);
    mpart.add_field("type", "content");

    builder.header(
        CONTENT_TYPE,
        format!("multipart/form-data; boundary={}", mpart.get_boundary()),
    );

    builder
        .uri(&url)
        .method(Method::POST)
        .body(Body::Streaming(Box::new(mpart.from_err())))
        .unwrap()
        .send()
        .timeout(Duration::from_secs(600))
        .from_err()
        .and_then(|resp| Ok(HttpResponse::build(resp.status()).finish()))
        .responder()
}
```

### Hyper > 0.11

* Create an mpart struct
* Set the boundary header
* Use the Body::pair method to stream it out

```rust

//Creates a multipart request with a randomly generated boundary
let mut mpart = MultipartRequest::default();

//Adds a Text Field to the multipart form
mpart.add_field("key", "value");

let mut request: Request<Body> = Request::new(Method::Post, request_url.parse()?);

{
    let headers = request.headers_mut();
    let mime = format!("multipart/form-data; boundary={}", mpart.get_boundary());
    
    //make sure you set the boundary header
    headers.set(ContentType(mime::Mime::from_str(&mime).unwrap()));
}

let (tx, body) = Body::pair();

let file_stream = mpart
    .map(|bytes| Ok(hyper::Chunk::from(bytes)))
    .map_err(|err| {
        error!("Could not stream file: {}", err);
        //Waiting for https://github.com/alexcrichton/futures-rs/issues/587
        unimplemented!()
    });

handle.spawn(tx.send_all(file_stream).map(|_| ()).map_err(|_| ()));

request.set_body(body);
```


