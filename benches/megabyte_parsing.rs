use bytes::{Bytes, BytesMut};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use futures_core::Stream;
use futures_util::StreamExt;
use mpart_async::client::{ByteStream, MultipartRequest};
use mpart_async::server::MultipartStream;
use rand::{thread_rng, RngCore};
use std::pin::Pin;
use std::task::{Context, Poll};

fn ten_megabytes_zero_byte(c: &mut Criterion) {
    let total_size = 1024 * 1024 * 10;

    let (bytes, boundary) = bytes_and_boundary(&['\0' as u8; 1024 * 1024 * 10]);

    let mut group = c.benchmark_group("ten megabytes zero byte");

    // with bigger chunk sizes this does get a lot of faster, but it would perhaps be better to
    // have multiple smaller files
    for chunk_size in &[512, 1024, 2048] {
        group.throughput(criterion::Throughput::Bytes(total_size as u64));

        group.bench_with_input(
            criterion::BenchmarkId::from_parameter(chunk_size),
            &total_size,
            |b, &size| {
                b.iter(|| {
                    let bytes = black_box(count_single_field_bytes(
                        boundary.clone(),
                        bytes.clone(),
                        size,
                    ));

                    assert_eq!(bytes, total_size);
                });
            },
        );
    }
}

fn ten_megabytes_r_byte(c: &mut Criterion) {
    let total_size = 1024 * 1024 * 10;

    let (bytes, boundary) = bytes_and_boundary(&['\r' as u8; 1024 * 1024 * 10]);

    let mut group = c.benchmark_group("ten megabytes \\r byte");

    // with bigger chunk sizes this does get a lot of faster, but it would perhaps be better to
    // have multiple smaller files
    for chunk_size in &[512, 1024, 2048] {
        group.throughput(criterion::Throughput::Bytes(total_size as u64));

        group.bench_with_input(
            criterion::BenchmarkId::from_parameter(chunk_size),
            &total_size,
            |b, &size| {
                b.iter(|| {
                    let bytes = black_box(count_single_field_bytes(
                        boundary.clone(),
                        bytes.clone(),
                        size,
                    ));

                    assert_eq!(bytes, total_size);
                });
            },
        );
    }
}

fn ten_megabytes_random(c: &mut Criterion) {
    let total_size = 1024 * 1024 * 10;

    let mut bytes_mut = BytesMut::with_capacity(total_size);

    bytes_mut.extend_from_slice(&['\0' as u8; 1024 * 1024 * 10]);

    thread_rng().fill_bytes(&mut bytes_mut);

    let (bytes, boundary) = bytes_and_boundary(&bytes_mut);

    let mut group = c.benchmark_group("ten megabytes random");

    // with bigger chunk sizes this does get a lot of faster, but it would perhaps be better to
    // have multiple smaller files
    for chunk_size in &[512, 1024, 2048] {
        group.throughput(criterion::Throughput::Bytes(total_size as u64));

        group.bench_with_input(
            criterion::BenchmarkId::from_parameter(chunk_size),
            &total_size,
            |b, &size| {
                b.iter(|| {
                    let bytes = black_box(count_single_field_bytes(
                        boundary.clone(),
                        bytes.clone(),
                        size,
                    ));

                    assert_eq!(bytes, total_size);
                });
            },
        );
    }
}

fn bytes_and_boundary(input: &[u8]) -> (Bytes, Bytes) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut request = MultipartRequest::default();

    let byte_stream = ByteStream::new(input);

    request.add_stream("name", "filename", "content_stream", byte_stream);

    let boundary = Bytes::from(request.get_boundary().to_string());

    let bytes = rt
        .block_on(request.fold(BytesMut::new(), |mut buf, result| async {
            if let Ok(bytes) = result {
                buf.extend_from_slice(&bytes);
            };

            buf
        }))
        .freeze();

    (bytes, boundary)
}

fn count_single_field_bytes(boundary: Bytes, bytes: Bytes, chunk_size: usize) -> usize {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let stream = ChunkedStream(bytes, chunk_size);
    let mut stream = MultipartStream::new(boundary, stream);

    let mut field = rt.block_on(stream.next()).unwrap().unwrap();
    let mut bytes = 0;
    loop {
        match rt.block_on(field.next()) {
            Some(Ok(read)) => bytes += read.len(),
            Some(Err(e)) => panic!("failed: {}", e),
            None => {
                break;
            }
        }
    }

    assert!(matches!(rt.block_on(stream.next()), None));
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

criterion_group!(
    benches,
    ten_megabytes_zero_byte,
    ten_megabytes_r_byte,
    ten_megabytes_random
);
criterion_main!(benches);
