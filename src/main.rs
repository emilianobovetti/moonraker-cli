use serde::Serialize;
use serde_json::json;
use std::env;
use std::io::{self, IsTerminal, Write};
use tokio::sync::mpsc::{self, Receiver, Sender};
use uuid::Uuid;

type JSON = serde_json::value::Value;

#[allow(unused)]
#[derive(Debug)]
enum Error {
    Request(reqwest::Error),
    Serde(serde_json::Error),
    JoinError(tokio::task::JoinError),
    SendError(mpsc::error::SendError<String>),
    IO(io::Error),
    Env(String),
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Error::Request(err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::IO(err)
    }
}

impl From<mpsc::error::SendError<String>> for Error {
    fn from(err: mpsc::error::SendError<String>) -> Self {
        Error::SendError(err)
    }
}

#[derive(Serialize)]
struct MoonrakerRPC<'a> {
    jsonrpc: &'a str,
    id: Uuid,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<JSON>,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    if !stdin.is_terminal() {
        return Err::<(), Error>(Error::Env(
            "Input device must be a TTY in interactive mode".to_string(),
        ));
    }

    let (io_tx, io_rx) = mpsc::channel::<String>(2);
    let (network_tx, mut network_rx) = mpsc::channel::<String>(2);

    let io_thread = tokio::task::spawn_blocking(move || -> Result<(), Error> {
        loop {
            stdout.write_all(b"> ")?;
            stdout.flush()?;

            let mut buffer = String::new();
            stdin.read_line(&mut buffer)?;

            io_tx.blocking_send(buffer)?;

            network_rx
                .blocking_recv()
                .map(|resp| stdout.write_fmt(format_args!("{}\n", resp)))
                .transpose()?;
        }
    });

    let args: Vec<String> = env::args().collect();
    let default_url = "http://localhost:7125".to_string();
    let url = args.get(1).unwrap_or(&default_url);

    tokio::select! {
        io_res = io_thread =>  { io_res.map_err(Error::JoinError).and_then(|res| res) }
        network_res = network_loop(url, network_tx, io_rx) => { network_res }
    }
}

async fn network_loop(
    url: &String,
    network_tx: Sender<String>,
    mut io_rx: Receiver<String>,
) -> Result<(), Error> {
    let client = reqwest::Client::new();

    loop {
        let input = io_rx.recv().await;

        let req = MoonrakerRPC {
            jsonrpc: "2.0",
            id: uuid::Uuid::new_v4(),
            method: "printer.gcode.script",
            params: Some(json!({ "script": input })),
        };

        let resp = client
            .post(format!("{}/server/jsonrpc", url))
            .json(&req)
            .send()
            .await?
            .json::<JSON>()
            .await
            .map_err(Error::Request)
            .and_then(format_json)?;

        network_tx.send(resp).await?;
    }
}

fn format_json(value: JSON) -> Result<String, Error> {
    serde_json::to_string_pretty(&value).map_err(Error::Serde)
}
