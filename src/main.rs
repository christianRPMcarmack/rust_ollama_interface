use futures_util::{SinkExt, StreamExt};
use poem::{
    get, handler,
    listener::TcpListener,
    web::{
        websocket::{Message, WebSocket},
        Data, Html,
    },
    EndpointExt, IntoResponse, Route, Server,
};
use reqwest::{Client, Response, Body};
use tokio::time::{timeout, Duration};
use tracing::{debug, error};
use wake_on_lan;

extern crate serde_json;
use serde_json::Value;

const OLLAMA_SERVER_MAC_ADDRESS: [u8; 6] = [0x0F, 0x1E, 0x2D, 0x3C, 0x4B, 0x5A];
const OLLAMA_URL: &str = "http://192.168.0.32:11434/api/generate";

#[allow(dead_code)]
const WEBSOCKET_URL: &str = "wss://chat.carmacks.org";

const TIMEOUT_DURATION: Duration = Duration::from_secs(5);

#[handler]
fn index() -> Html<&'static str> {
    Html(
        r###"
        <body style="display: flex; justify-content: center; align-items: center; min-height: 100vh;">
            <div style="text-align: center;">
                <div id="sendFormContainer">
                    <form id="sendForm">
                        <input id="msgInput" type="text" />
                        <button type="submit">Ask</button>
                    </form>
                    <textarea id="msgsArea" cols="50" rows="30"></textarea>
                </div>
            </div>
        </body>
        <script>
            let ws;
            const sendForm = document.querySelector("#sendForm");
            const msgInput = document.querySelector("#msgInput");
            const msgsArea = document.querySelector("#msgsArea");
        
            ws = new WebSocket("wss://chat.carmacks.org/ws/conversation");
            ws.onmessage = function(event) {
                msgsArea.value += event.data;
                // Automatically scroll to the bottom
                msgsArea.scrollTop = msgsArea.scrollHeight;
                // Resize the text area to fit the content
                msgsArea.style.height = msgsArea.scrollHeight + "px";
            }
            sendForm.addEventListener("submit", function(event) {
                event.preventDefault();
                ws.send(msgInput.value);
                msgInput.value = "";
            });
        
            // Set the initial width of the text area
            msgsArea.style.width = "80%"; // Adjust the width as needed
        </script>
        "###,
    )
}

#[handler]
fn ws(
    ws: WebSocket,
    sender: Data<&tokio::sync::broadcast::Sender<String>>,
) -> impl IntoResponse {
    let sender = sender.clone();
    let mut receiver = sender.subscribe();
    ws.on_upgrade(move |socket| async move {
        let (mut sink, mut stream) = socket.split();

        tokio::spawn(async move {

            if sender.send("Connected...".to_string()).is_err() {
                error!("Failed");
            }
            let mut context: Vec<i64> = vec![0];
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Text(text) = msg {
                    debug!("Received text: {}", text);
                    if sender.send(format!("\n\nYou: {text}")).is_err() {
                        break;
                    }

                    // Send the message to ollama and print the response
                    if ping_ollama().await {
                        if let Ok(response) = send_to_ollama(&text, &context).await {
                            if let Some(conversation_id) = print_response(response, &sender).await {
                                context = conversation_id;
                            }
                        }
                    } else {
                        if sender.send("System: Ollama server is offline...wait for ping to wake server and try again in 10 seconds ".to_string()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        tokio::spawn(async move {
            while let Ok(msg) = receiver.recv().await {
                if sink.send(Message::Text(msg)).await.is_err() {
                    break;
                }
            }
        });
    })
}

async fn send_to_ollama(message: &str, context: &Vec<i64>) -> Result<Response, reqwest::Error> {
    let mut body = Body::from(serde_json::json!({ "model": "llama2-uncensored", "prompt": message, "context": context }).to_string());
    if context.is_empty() {
        body = Body::from(serde_json::json!({ "model": "llama2-uncensored", "prompt": message }).to_string());
    }

    let client = Client::new();
    let response = client
        .post(OLLAMA_URL)
        .body(body)
        .send()
        .await?;
    Ok(response)
}

async fn ping_ollama() -> bool {
    let client = Client::new();
    let response = timeout(
        TIMEOUT_DURATION,
        client.get(OLLAMA_URL).send(),
    )
        .await;

    match response {
        Ok(res) => match res {
            Ok(_) => true,
            Err(err) => {
                error!("Error trying to ping Ollama: {}", err.to_string());
                false
            }
        },
        Err(_) => {
            error!("Timed out trying to ping Ollama");
            let magic_packet = wake_on_lan::MagicPacket::new(&OLLAMA_SERVER_MAC_ADDRESS);
            let _ = magic_packet.send();
            false
        }
    }
}

fn extract_responses(json_strs: &str) -> String {
    let mut responses = String::new();
    for line in json_strs.lines() {
        let json_value: Value = serde_json::from_str(line).unwrap();
        if let Some(response) = json_value.get("response") {
            if let Some(response_str) = response.as_str() {
                responses.push_str(response_str);
            }
        }
    }
    responses
}

async fn print_response(
    mut response: Response,
    sender: &tokio::sync::broadcast::Sender<String>,
) -> Option<Vec<i64>> {
    let mut context: Vec<i64> = vec![0];
    let mut response_text = String::new();
    if sender.send("\n\nOllama: ".to_string()).is_err() {
        return None;
    }
    while let Some(chunk) = response.chunk().await.unwrap() {
        response_text.push_str(&String::from_utf8_lossy(&chunk));
        let response = extract_responses(&response_text);
        if response.is_empty() {
            let json: Value = serde_json::from_str(&response_text).unwrap();
            context = serde_json::from_value(json["context"].clone()).unwrap();
        }
        if sender.send(format!("{}", response)).is_err() {
            return None;
        }
        response_text.clear();
    }
    return Some(context);
}


const WEBSOCKET_PATH: &str = "/ws/:app";
#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "poem=debug");
    }
    tracing_subscriber::fmt()
        .with_env_filter("debug")
        .init();

    let app = Route::new()
        .at("/", get(index))
        .at(WEBSOCKET_PATH, get(ws.data(tokio::sync::broadcast::channel::<String>(32).0)));

    Server::new(TcpListener::bind("0.0.0.0:3010"))
        .run(app)
        .await
}