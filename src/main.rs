use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::ExecutableCommand;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Span, Text};
use ratatui::widgets::{List, ListDirection, ListState, Widget};
use serde::Serialize;
use serde_json::json;
use std::time::Duration;
use std::{env, io};
use tokio::sync::mpsc::error::TryRecvError;
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
    DisconnectedIOChannel,
    DisconnectedNetworkChannel,
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

struct CommandInput<'a> {
    prompt: &'a str,
    input: String,
}

impl CommandInput<'_> {
    const fn new() -> Self {
        Self {
            prompt: "> ",
            input: String::new(),
        }
    }

    fn on_key_press(&mut self, event: KeyEvent) {
        match event.code {
            KeyCode::Char(ch) => self.input.push(ch),
            KeyCode::Backspace => {
                self.input.pop();
            }
            _ => {}
        }
    }

    fn len(&self) -> usize {
        self.prompt.chars().count() + self.input.chars().count()
    }

    fn cursor_position(&self, area: Rect) -> Position {
        let input_len = self.len() as u16;

        Position::new(input_len % area.width, area.height)
    }

    fn lines_count(&self, area: Rect) -> u16 {
        (self.len() as u16 / area.width) + 1
    }
}

impl Widget for &CommandInput<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width = area.width as usize;
        let prompt_len = self.prompt.chars().count() as u16;
        let mut line_area = Rect { height: 1, ..area };

        Span::from(self.prompt).bold().render(
            Rect {
                width: prompt_len,
                ..line_area
            },
            buf,
        );

        // TODO: unicode support
        let mut chars = self.input.chars();

        chars
            .by_ref()
            .take(width - self.prompt.len())
            .collect::<String>()
            .render(
                Rect {
                    x: prompt_len,
                    ..line_area
                },
                buf,
            );

        loop {
            let line = chars.by_ref().take(width).collect::<String>();

            if line.is_empty() {
                break;
            }

            line_area.y += 1;
            line.render(line_area, buf);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let (io_tx, io_rx) = mpsc::channel::<String>(2);
    let (network_tx, mut network_rx) = mpsc::channel::<String>(2);

    let mut terminal = ratatui::init();
    let mut cmd = CommandInput::new();
    let mut responses: Vec<String> = Vec::new();
    let mut list_state = ListState::default();

    std::io::stdout().execute(event::EnableMouseCapture)?;

    let io_thread = tokio::task::spawn_blocking(move || -> Result<(), Error> {
        loop {
            terminal.draw(|frame| {
                let area = frame.area();
                let layout = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(cmd.lines_count(area)),
                ]);

                let [output_area, input_area] = layout.areas(area);

                let lines: Vec<_> = responses
                    .iter()
                    .rev()
                    .map(|resp| Text::raw(resp.as_str()))
                    .collect();

                let list = List::new(lines).direction(ListDirection::BottomToTop);

                frame.render_stateful_widget(list, output_area, &mut list_state);
                frame.render_widget(&cmd, input_area);
                frame.set_cursor_position(cmd.cursor_position(area));
            })?;

            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('c'),
                        modifiers: KeyModifiers::CONTROL,
                        ..
                    }) => break,
                    Event::Key(KeyEvent {
                        code: KeyCode::Esc, ..
                    }) => (),
                    Event::Key(KeyEvent {
                        code: KeyCode::Enter,
                        ..
                    }) => {
                        let input = cmd.input;
                        cmd.input = String::new();
                        io_tx.blocking_send(input)?
                    }
                    Event::Key(event) if event.kind == KeyEventKind::Press => {
                        cmd.on_key_press(event)
                    }
                    Event::Key(input) => todo!("Key event"),
                    Event::FocusGained => todo!("FocusGained event"),
                    Event::FocusLost => todo!("FocusLost event"),
                    // TODO: handle text selection and mouse scroll
                    Event::Mouse(event) => {}
                    Event::Paste(input) => todo!("Paste event"),
                    Event::Resize(_columns, _rows) => {}
                }
            }

            match network_rx.try_recv() {
                Ok(resp) => responses.push(resp),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    return Err(Error::DisconnectedNetworkChannel);
                }
            }
        }

        // TODO: ensure we are calling these when an error occurs
        std::io::stdout().execute(event::DisableMouseCapture)?;
        ratatui::restore();

        Ok(())
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
        let input = io_rx.recv().await.ok_or(Error::DisconnectedIOChannel)?;

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
