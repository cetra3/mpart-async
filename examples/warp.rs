use warp::Filter;

use bytes::Buf;
use futures::stream::TryStreamExt;
use futures::Stream;
use mime::Mime;
use mpart_async::MpartStream;
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

    let mut stream = MpartStream::new(boundary, body.map_ok(|mut buf| buf.to_bytes()));

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
