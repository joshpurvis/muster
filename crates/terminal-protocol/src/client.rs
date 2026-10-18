//! Blocking local control client. Streaming connections are independent of control calls.
//! Never retries a mutation after a lost response. Callers reconcile its outcome first.
use crate::{
    Command, Hello, Request, Response, VERSION,
    wire::{self, Frame},
};

#[derive(Debug)]
pub struct HostError {
    pub code: String,
    pub message: String,
}
impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "terminal host ({}): {}", self.code, self.message)
    }
}
impl std::error::Error for HostError {}
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    pub hello: Hello,
}

impl Client {
    pub fn connect(socket: &Path) -> anyhow::Result<Self> {
        let mut stream = connect(socket)?;
        wire::write(
            &mut stream,
            &Frame::json(&Request {
                version: VERSION,
                instance: None,
                command: Command::Hello,
            })?,
        )?;
        let response: Response = wire::read(&mut stream)?.decode()?;
        match checked(response)? {
            Response::Hello(hello) => {
                anyhow::ensure!(
                    hello.protocol == VERSION,
                    "unsupported terminal host protocol {}",
                    hello.protocol
                );
                Ok(Self {
                    socket: socket.to_owned(),
                    hello,
                })
            }
            _ => anyhow::bail!("unexpected host handshake response"),
        }
    }

    pub fn call(&self, command: Command) -> anyhow::Result<Response> {
        let mut stream = self.request(command)?;
        checked(wire::read(&mut stream)?.decode()?)
    }

    pub fn subscribe(&self, command: Command) -> anyhow::Result<UnixStream> {
        anyhow::ensure!(
            matches!(command, Command::Attach { .. } | Command::Events),
            "not a streaming operation"
        );
        let mut stream = self.request(command)?;
        match checked(wire::read(&mut stream)?.decode()?)? {
            Response::Attached => {
                stream.set_read_timeout(Some(Duration::from_secs(30)))?;
                Ok(stream)
            }
            _ => anyhow::bail!("unexpected stream subscription response"),
        }
    }

    /// Open a persistent input connection (`Command::InputStream`). A host that predates
    /// it closes the connection without answering; nothing was sent, so callers may fall
    /// back to one `call` per input.
    pub fn input_stream(&self) -> anyhow::Result<UnixStream> {
        let mut stream = self.request(Command::InputStream)?;
        match checked(wire::read(&mut stream)?.decode()?)? {
            Response::Attached => Ok(stream),
            _ => anyhow::bail!("unexpected input stream response"),
        }
    }

    /// Send one input on a stream from `input_stream` and wait for its answer. A failed write
    /// means the host never received the whole frame (it was not applied); a failure after
    /// the frame was written leaves the outcome unknown: drop the stream, never resend.
    pub fn stream_input(stream: &mut UnixStream, command: &Command) -> Result<(), StreamError> {
        let frame = Frame::json(&Request { version: VERSION, instance: None, command: command.clone() })
            .map_err(StreamError::Unsent)?;
        wire::write(stream, &frame).map_err(StreamError::Unsent)?;
        let response = wire::read(stream).and_then(|frame| frame.decode::<Response>()).map_err(StreamError::Lost)?;
        checked(response).map(|_| ()).map_err(StreamError::Host)
    }

    /// One input on its own connection (a host without input streams), classified like
    /// `stream_input`: a failure to connect or to write the whole frame never applied it; a
    /// failure after the frame was written leaves the outcome unknown.
    pub fn call_input(&self, command: &Command) -> Result<(), StreamError> {
        let mut stream = connect(&self.socket).map_err(StreamError::Unsent)?;
        let frame = Frame::json(&Request {
            version: VERSION,
            instance: Some(self.hello.instance.clone()),
            command: command.clone(),
        })
        .map_err(StreamError::Unsent)?;
        wire::write(&mut stream, &frame).map_err(StreamError::Unsent)?;
        let response = wire::read(&mut stream).and_then(|frame| frame.decode::<Response>()).map_err(StreamError::Lost)?;
        checked(response).map(|_| ()).map_err(StreamError::Host)
    }

    fn request(&self, command: Command) -> anyhow::Result<UnixStream> {
        let mut stream = connect(&self.socket)?;
        wire::write(
            &mut stream,
            &Frame::json(&Request {
                version: VERSION,
                instance: Some(self.hello.instance.clone()),
                command,
            })?,
        )?;
        Ok(stream)
    }
}

/// Outcome of `Client::stream_input` and `Client::call_input`.
#[derive(Debug)]
pub enum StreamError {
    /// The host did not receive the complete input (for example, the stream had closed).
    Unsent(anyhow::Error),
    /// The host answered with an error; the stream remains usable.
    Host(anyhow::Error),
    /// The connection failed after the input may have been sent: outcome unknown.
    Lost(anyhow::Error),
}

fn connect(socket: &Path) -> anyhow::Result<UnixStream> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

fn checked(response: Response) -> anyhow::Result<Response> {
    match response {
        Response::Error { code, message } => Err(HostError { code, message }.into()),
        value => Ok(value),
    }
}
