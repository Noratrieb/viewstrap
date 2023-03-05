#[macro_use]
extern crate tracing;

use std::{net::SocketAddr, path::PathBuf};

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket},
        ConnectInfo, TypedHeader, WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use bootstrap::Compiler;
use futures::{stream::StreamExt, SinkExt};
use serde::{Deserialize, Serialize};
use tower_http::{
    services::ServeDir,
    trace::{DefaultMakeSpan, TraceLayer},
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod bootstrap;

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "viewstrap=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("bootstrapping viewstrap...");

    let Some(rustc_src_dir) = std::env::args().nth(1) else {
        error!("most provide rust source dir as the first argument");
        std::process::exit(1);
    };

    let entrypoint = PathBuf::from(rustc_src_dir).join(if cfg!(windows) { "x.ps1" } else { "x" });
    if !entrypoint.exists() {
        error!("{} not found, your rust source is either really old or not a rust source and you lied to me.", entrypoint.display());
        std::process::exit(1);
    }

    let assets_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");

    // build our application with some routes
    let app = Router::new()
        .fallback_service(ServeDir::new(assets_dir).append_index_html_on_directories(true))
        .route(
            "/ws",
            get(move |ws, user_agent, addr| ws_handler(ws, user_agent, addr, entrypoint.clone())),
        )
        // logging so we can see whats going on
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

/// The handler for the HTTP request (this gets called when the HTTP GET lands at the start
/// of websocket negotiation). After this completes, the actual switching from HTTP to
/// websocket protocol will occur.
/// This is the last point where we can extract TCP/IP metadata such as IP address of the client
/// as well as things from HTTP headers such as user-agent of the browser etc.
async fn ws_handler(
    ws: WebSocketUpgrade,
    user_agent: Option<TypedHeader<headers::UserAgent>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    entrypoint: PathBuf,
) -> impl IntoResponse {
    let user_agent = if let Some(TypedHeader(user_agent)) = user_agent {
        user_agent.to_string()
    } else {
        String::from("Unknown browser")
    };
    println!("`{user_agent}` at {addr} connected.");
    // finalize the upgrade process by returning upgrade callback.
    // we can customize the callback by sending additional info such as address.
    ws.on_upgrade(move |socket| handle_socket(socket, addr, entrypoint))
}

#[derive(Debug, Clone, Serialize)]
enum ServerMessage<'a> {
    Stdout(&'a str),
    Stderr(&'a str),
    AvailableCompilers(Vec<Compiler>),
}

#[derive(Debug, Clone, Deserialize)]
enum ClientMessage {
    Compile,
    ListCompilers,
}

impl<'a> From<ServerMessage<'a>> for Message {
    fn from(value: ServerMessage<'a>) -> Self {
        let text = serde_json::to_string(&value).unwrap();
        Message::Text(text)
    }
}

/// Actual websocket statemachine (one will be spawned per connection)
async fn handle_socket(mut socket: WebSocket, who: SocketAddr, entrypoint: PathBuf) {
    //send a ping (unsupported by some browsers) just to kick things off and get a response
    if socket.send(Message::Ping(vec![1, 2, 3])).await.is_ok() {
        info!("Pinged {}...", who);
    } else {
        info!("Could not send ping {}!", who);
        // no Error here since the only thing we can do is to close the connection.
        // If we can not send messages, there is no way to salvage the statemachine anyway.
        return;
    }

    // receive single message from a client (we can either receive or send with socket).
    // this will likely be the Pong for our Ping or a hello message from client.
    // waiting for message from a client will block this task, but will not block other client's
    // connections.
    if let Some(msg) = socket.recv().await {
        if let Ok(msg) = msg {
            info!(?msg);
        } else {
            info!("client {who} abruptly disconnected");
            return;
        }
    }

    // By splitting socket we can send and receive at the same time. In this example we will send
    // unsolicited messages to client based on some sort of server's internal event (i.e .timer).
    let (mut sender, mut receiver) = socket.split();

    // This second task will receive messages from client and print them on server console
    while let Some(Ok(msg)) = receiver.next().await {
        info!(?msg);

        if let Message::Text(msg_str) = msg {
            let msg = serde_json::from_str::<ClientMessage>(&msg_str);

            match msg {
                Ok(ClientMessage::Compile) => {
                    if let Err(err) = bootstrap::build_a_compiler(&mut sender, &entrypoint).await {
                        error!(%err);
                    }
                }
                Ok(ClientMessage::ListCompilers) => {
                    let compilers = bootstrap::list_compilers(&entrypoint).await;
                    if let Err(err) = sender
                        .send(ServerMessage::AvailableCompilers(compilers).into())
                        .await
                    {
                        error!(%err);
                    }
                }
                Err(err) => {
                    error!(?err, ?msg_str, "invalid client message");
                    if let Err(err) = sender
                        .send(Message::Close(Some(CloseFrame {
                            code: 0,
                            reason: "invalid message, you naughty".into(),
                        })))
                        .await
                    {
                        error!(%err);
                    }
                }
            }
        }
    }

    // returning from the handler closes the websocket connection
    println!("Websocket context {} destroyed", who);
}
