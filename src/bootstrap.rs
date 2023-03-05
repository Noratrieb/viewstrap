use std::{path::Path, pin::Pin, process::Stdio};

use axum::extract::ws::{self, WebSocket};
use color_eyre::{eyre::Context, Result};
use futures::{stream::SplitSink, SinkExt};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

use crate::{Action, Compiler, ServerMessage};

pub async fn build_a_compiler(
    output: &mut SplitSink<WebSocket, ws::Message>,
    entrypoint: &Path,
    compiler: Compiler,
    action: Action,
) -> Result<()> {
    let cwd = entrypoint.parent().unwrap();

    let stage = match compiler {
        Compiler::Bootstrap => "0",
        Compiler::Dev => "1",
        Compiler::Dist => "2",
    };

    let action = match action {
        Action::BuildCompiler => "compiler",
        Action::BuildLibrary => "library",
    };

    let mut cmd = Command::new(entrypoint);
    cmd.current_dir(cwd);
    cmd.args(["build", "--stage", stage, action]);
    // cmd.arg("--help");

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    debug!(?cmd, "about to run command");

    let mut cmd = cmd.spawn().context("failed to spawn entrypoint")?;

    let stdout = cmd.stdout.take().unwrap();
    let stderr = cmd.stderr.take().unwrap();

    handle_stdouts(Box::pin(stdout), Box::pin(stderr), cmd, output).await?;

    Ok(())
}

pub async fn list_compilers(entrypoint: &Path) -> Vec<Compiler> {
    let mut compilers = vec![Compiler::Bootstrap];

    let build = entrypoint.parent().unwrap().join("build/host");

    if let Ok(true) = tokio::fs::try_exists(build.join("stage1").join("bin").join("rustc")).await {
        compilers.push(Compiler::Dev);
    }

    if let Ok(true) = tokio::fs::try_exists(build.join("stage2").join("bin").join("rustc")).await {
        compilers.push(Compiler::Dist);
    }

    compilers
}

async fn handle_stdouts(
    mut stdout: Pin<Box<impl AsyncRead>>,
    mut stderr: Pin<Box<impl AsyncRead>>,
    mut child: Child,
    output: &mut SplitSink<WebSocket, ws::Message>,
) -> Result<()> {
    // this is a horrible 0 AM hack

    let mut stdout_buf = [0; 1024];
    let mut stderr_buf = [0; 1024];

    loop {
        tokio::select! {
            stdout_read = stdout.read(&mut stdout_buf) => {
                match stdout_read {
                    Ok(0) => {}
                    Ok(len) => {
                        let read = std::str::from_utf8(&stdout_buf[0..len])?;
                        debug!("Read {len} bytes from stdout");
                        output.send(ServerMessage::Stdout(read).into()).await?;
                    }
                    Err(err) => {
                        return Err(err.into());
                    }
                }
            }
            stderr_read = stderr.read(&mut stderr_buf) => {
                match stderr_read {
                    Ok(0) => {}
                    Ok(len) => {
                        let read = std::str::from_utf8(&stderr_buf[0..len])?;
                        debug!("Read {len} bytes from stderr");
                        output.send(ServerMessage::Stderr(read).into()).await?;
                    }
                    Err(err) => {
                        return Err(err.into());
                    }
                }
            }
            _ = child.wait() => {
                info!("Child process finished");
                break;
            }
        }
    }

    let mut stdout_done = false;
    let mut stderr_done = true;

    loop {
        tokio::select! {
            stdout_read = stdout.read(&mut stdout_buf) => {
                match stdout_read {
                    Ok(0) => {
                        stdout_done = true;
                        if stderr_done {
                            break;
                        }
                    }
                    Ok(len) => {
                        let read = std::str::from_utf8(&stdout_buf[0..len])?;
                        output.send(ServerMessage::Stdout(read).into()).await?;
                    }
                    Err(err) => {
                        return Err(err.into());
                    }
                }
            }
            stderr_read = stderr.read(&mut stderr_buf) => {
                match stderr_read {
                    Ok(0) => {
                        stderr_done = true;
                        if stdout_done {
                            break;
                        }
                    }
                    Ok(len) => {
                        let read = std::str::from_utf8(&stderr_buf[0..len])?;
                        output.send(ServerMessage::Stderr(read).into()).await?;
                    }
                    Err(err) => {
                        return Err(err.into());
                    }
                }
            }
        }
    }

    Ok(())
}
