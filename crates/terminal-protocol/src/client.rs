//! Blocking local control client. Streaming connections are independent of control calls.
//! Never retries a mutation after a lost response. Callers reconcile its outcome first.
use crate::{
    Command, Hello, Request, Response, VERSION,
    wire::{self, Frame},
};
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

fn connect(socket: &Path) -> anyhow::Result<UnixStream> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

fn checked(response: Response) -> anyhow::Result<Response> {
    match response {
        Response::Error { code, message } => anyhow::bail!("terminal host ({code}): {message}"),
        value => Ok(value),
    }
}
