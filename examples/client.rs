extern crate env_logger;

extern crate tokio_core;
extern crate futures;
extern crate tokio_solicit;

use std::str;
use std::io::{self};

use futures::{Future, Stream};
use futures::future::{self};
use tokio_core::reactor::{Core};

use tokio_solicit::client::H2Client;

// Shows the usage of `H2Client` when establishing an HTTP/2 connection over cleartext TCP.
// Also demonstrates how to stream the body of the response (i.e. get response body chunks as soon
// as they are received on a `futures::Stream`.
fn cleartext_example() {
    let mut core = Core::new().expect("event loop required");
    let handle = core.handle();

    let addr = "127.0.0.1:8080".parse().expect("valid IP address");

    let future_client = H2Client::cleartext_connect("localhost", &addr, &handle);

    let future_response = future_client.and_then(|mut client| {
        println!("Connection established.");

        // For the first request, we simply want the full body, without streaming individual body
        // chunks...
        let get = client.get(b"/get").into_full_body_response();
        let post = client.post(b"/post", b"Hello, world!".to_vec());

        // ...for the other, we accumulate the body "manually" in order to do some more
        // processing for each chunk (for demo purposes). Also, discards the headers.
        let post = post.and_then(|(_headers, body)| {
            body.fold(Vec::<u8>::new(), |mut vec, chunk| {
                println!("receiving a new chunk of size {}", chunk.body.len());

                vec.extend(chunk.body.into_iter());
                future::ok::<_, io::Error>(vec)
            })
        });

        // Finally, yield a future that resolves once both requests are complete (and both bodies
        // are available).
        Future::join(get, post)
    }).map(|(get_response, post_response_body)| {
        // Convert the bodies to a UTF-8 string
        let get_res: String = str::from_utf8(&get_response.body).unwrap().into();
        let post_res: String = str::from_utf8(&post_response_body).unwrap().into();

        // ...and yield a pair of bodies converted to a string.
        (get_res, post_res)
    });

    let res = core.run(future_response).expect("responses!");

    println!("{:?}", res);
}

/// Fetches the response of google.com over HTTP/2.
///
/// The connection is negotiated during the TLS handshake using ALPN.
fn alpn_example() {
    println!();
    println!("---- ALPN example ----");
    use std::net::{ToSocketAddrs};

    let mut core = Core::new().expect("event loop required");
    let handle = core.handle();
    let addr =
        "google.com:443"
            .to_socket_addrs()
            .expect("unable to resolve the domain name")
            .next()
            .expect("no matching ip addresses");

    let future_response = H2Client::connect("google.com", &addr, &handle).and_then(|mut client| {
        // Ask for the homepage...
        client.get(b"/").into_full_body_response()
    });

    let response = core.run(future_response).expect("unexpected failure");
    // Print both the headers and the response body...
    println!("{:?}", response.headers);
    // (Recklessly assume it's utf-8!)
    let body = str::from_utf8(&response.body).unwrap();
    println!("{}", body);

    println!("---- ALPN example end ----");
}

fn main() {
    env_logger::init().expect("logger init is required");

    // Establish an http/2 connection (over cleartext TCP), issue a couple of requests, and
    // stream the body of the response of one of them. Wait until both are ready and then
    // print the bodies.
    cleartext_example();

    // Show how to estalbish a connection over TLS, while negotiating the use of http/2 over ALPN.
    alpn_example();

    // An additional demo showing how to perform a streaming _request_ (i.e. the body of the
    // request is streamed out to the server).
    do_streaming_request();
}

fn do_streaming_request() {
    println!();
    println!("---- streaming request example ----");
    use std::iter;
    use tokio_solicit::client::HttpRequestBody;
    use futures::Sink;

    let mut core = Core::new().expect("event loop required");
    let handle = core.handle();
    let addr = "127.0.0.1:8080".parse().expect("valid IP address");
    let future_client = H2Client::cleartext_connect("localhost", &addr, &handle);

    let future_response = future_client.and_then(|mut client| {
        let (post, tx) = client.streaming_request(b"POST", b"/post", iter::empty());
        tx
            .send(Ok(HttpRequestBody::new(b"HELLO ".to_vec())))
            .and_then(|tx| tx.send(Ok(HttpRequestBody::new(b" WORLD".to_vec()))))
            .and_then(|tx| tx.send(Ok(HttpRequestBody::new(b"!".to_vec()))))
            .map_err(|_err| io::Error::from(io::ErrorKind::BrokenPipe))
            .and_then(|_tx| post.into_full_body_response())
    });

    let res = core.run(future_response).expect("response");
    println!("{:?}", res);
    println!("---- streaming request example end ----");
}
