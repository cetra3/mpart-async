use bytes::{BufMut, Bytes, BytesMut};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use futures::executor::block_on;
use futures::stream::{Stream, StreamExt};
use mpart_async::server::MultipartStream;
use std::pin::Pin;
use std::task::{Context, Poll};

fn criterion_benchmark(c: &mut Criterion) {
    let boundary = b"----------------------------332056022174478975396798";

    let mut buffer = Vec::with_capacity(10 * 1024 * 1024);
    buffer.extend_from_slice(b"--");
    buffer.extend_from_slice(boundary);
    buffer.extend_from_slice(b"\r\n");
    buffer.extend_from_slice(b"Content-Disposition: form-data; name=\"file\"\r\n");
    buffer.extend_from_slice(b"Content-Type: application/octet-stream\r\n");
    buffer.extend_from_slice(b"\r\n");

    // formidable uses just zeroes so I guess that's good enough:
    // https://github.com/node-formidable/formidable/blob/5110ef8ddb78501dcedbdcb7e2754d94abe06bc5/benchmark/index.js#L45

    let mut zeroes = BytesMut::with_capacity(1024);
    for _ in 0..(zeroes.capacity() / 8) {
        zeroes.put_u64(0);
    }

    let trailer = 2 + boundary.len() + 2;
    let target = buffer.capacity();
    let mut remaining = target - (buffer.len() + trailer);
    let zeroes_used = remaining;

    while remaining > zeroes.len() {
        buffer.extend_from_slice(zeroes.as_ref());
        remaining -= zeroes.len();
    }

    buffer.extend_from_slice(&zeroes.as_ref()[..remaining]);

    buffer.extend_from_slice(b"\r\n--");
    buffer.extend_from_slice(boundary);
    buffer.extend_from_slice(b"--\r\n");

    let boundary: Bytes = (&boundary[..]).into();
    let bytes: Bytes = buffer.into();

    let mut group = c.benchmark_group("ten megabytes");

    // with bigger chunk sizes this does get a lot of faster, but it would perhaps be better to
    // have multiple smaller files
    for chunk_size in &[512] {
        group.throughput(criterion::Throughput::Bytes(target as u64));

        group.bench_with_input(
            criterion::BenchmarkId::from_parameter(chunk_size),
            &zeroes_used,
            |b, &size| {
                b.iter(|| {
                    let bytes = black_box(count_single_field_bytes(
                        boundary.clone(),
                        bytes.clone(),
                        size,
                    ));

                    assert_eq!(bytes, zeroes_used);
                });
            },
        );
    }
}

fn count_single_field_bytes(boundary: Bytes, bytes: Bytes, chunk_size: usize) -> usize {
    let stream = ChunkedStream(bytes, chunk_size);
    let mut stream = MultipartStream::new(boundary, stream);

    let mut field = block_on(stream.next()).unwrap().unwrap();
    let mut bytes = 0;
    loop {
        match block_on(field.next()) {
            Some(Ok(read)) => bytes += read.len(),
            Some(Err(e)) => panic!("failed: {}", e),
            None => {
                break;
            }
        }
    }

    assert!(matches!(block_on(stream.next()), None));
    bytes
}

struct ChunkedStream(Bytes, usize);

impl Stream for ChunkedStream {
    type Item = Result<Bytes, std::convert::Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let amt = self.1.min(self.0.len());

        if amt > 0 {
            let bytes = self.as_mut().0.split_to(amt);
            Poll::Ready(Some(Ok(bytes)))
        } else {
            Poll::Ready(None)
        }
    }
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
