//! A daemon on the other end of a socket pair, for the tests of the bus's
//! clients: every call it heard, whole, and the answer each test gives.

use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use futures_lite::StreamExt as _;

/// The client end of a daemon answering each call as `answer` says (`None`:
/// never answered), and every call it heard, before its answer.
pub(crate) fn serving(
    answer: impl Fn(&zbus::Message) -> Option<zbus::Message> + Send + 'static,
) -> (zbus::Connection, mpsc::Receiver<zbus::Message>) {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let (server, client) = futures_lite::future::block_on(futures_lite::future::zip(
        zbus::connection::Builder::async_io_unix_stream(server_end)
            .p2p()
            .server(zbus::Guid::generate())
            .unwrap()
            .build(),
        zbus::connection::Builder::async_io_unix_stream(client_end)
            .p2p()
            .build(),
    ));
    let server = server.unwrap();
    // Listened to before the client sends anything.
    let mut messages = zbus::MessageStream::from(&server);
    let (told, heard) = mpsc::channel();
    std::thread::spawn(move || {
        futures_lite::future::block_on(async move {
            while let Some(Ok(message)) = messages.next().await {
                if message.header().message_type() != zbus::message::Type::MethodCall {
                    continue;
                }
                let answered = answer(&message);
                // The test may be done with what was heard.
                let _ = told.send(message);
                if let Some(reply) = answered {
                    server.send(&reply).await.unwrap();
                }
            }
        });
    });
    (client.unwrap(), heard)
}

/// The reply to `call`, of `body`.
#[expect(
    clippy::unnecessary_wraps,
    reason = "an answer's shape: `None` answers nothing"
)]
pub(crate) fn reply<B>(call: &zbus::Message, body: &B) -> Option<zbus::Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    Some(
        zbus::Message::method_return(&call.header())
            .unwrap()
            .build(body)
            .unwrap(),
    )
}

/// The message of every error the daemon answers.
pub(crate) const WORDS: &str = "the daemon's words";

/// The error `name` in reply to `call`, said in [`WORDS`].
#[expect(
    clippy::unnecessary_wraps,
    reason = "an answer's shape: `None` answers nothing"
)]
pub(crate) fn error(call: &zbus::Message, name: &str) -> Option<zbus::Message> {
    Some(
        zbus::Message::error(&call.header(), name)
            .unwrap()
            .build(&WORDS)
            .unwrap(),
    )
}

/// Where `call` was sent: destination, object, interface, member, and the
/// signature of its body.
pub(crate) fn addressed(call: &zbus::Message) -> [String; 5] {
    let header = call.header();
    let text = |part: Option<String>| part.unwrap_or_default();
    [
        text(header.destination().map(ToString::to_string)),
        text(header.path().map(ToString::to_string)),
        text(header.interface().map(ToString::to_string)),
        text(header.member().map(ToString::to_string)),
        call.body().signature().to_string(),
    ]
}
