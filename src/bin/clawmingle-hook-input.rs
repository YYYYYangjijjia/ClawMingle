//! Fixed exec-server input adapter, mounted only inside the scoped executor.
//! A length frame avoids depending on a process/write stdin-close operation.

use std::{
    io::{self, Read, Write},
    process::{Command, ExitCode, Stdio},
};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(_) => {
            eprintln!("ClawMingle hook input failed");
            ExitCode::FAILURE
        }
    }
}

fn read_frame(mut reader: impl Read) -> io::Result<(String, Vec<u8>)> {
    let mut header = [0_u8; 8];
    reader.read_exact(&mut header)?;
    let command_size = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    let input_size = u32::from_be_bytes(header[4..].try_into().unwrap()) as usize;
    if command_size == 0 || command_size > 65_536 || input_size > 1024 * 1024 {
        return Err(io::Error::other("invalid frame size"));
    }
    let mut command = vec![0; command_size];
    reader.read_exact(&mut command)?;
    let command = String::from_utf8(command).map_err(|_| io::Error::other("invalid command"))?;
    if command.contains('\0') {
        return Err(io::Error::other("invalid command"));
    }
    let mut input = vec![0; input_size];
    reader.read_exact(&mut input)?;
    Ok((command, input))
}

fn run() -> io::Result<u8> {
    if std::env::args_os().len() != 1 {
        return Err(io::Error::other("unexpected arguments"));
    }
    let (command, input) = read_frame(io::stdin().lock())?;
    // This executable is never mounted in the authority. It inherits only the
    // fixed executor environment, and /bin/sh is the executor's real shell.
    let mut child = Command::new("/bin/sh")
        .args(["-lc", &command])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let written = child.stdin.take().unwrap().write_all(&input);
    // Dropping the child's stdin above delivers EOF, even while the parent
    // process/write connection remains open. exec-server owns tree cleanup.
    if let Err(error) = written
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child
        .wait()?
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_preserves_native_json_bytes_and_needs_no_transport_eof() {
        let command = b"cat";
        let input = b"{\"line\":\"a\\nb\"}";
        let mut frame = Vec::new();
        frame.extend_from_slice(&(command.len() as u32).to_be_bytes());
        frame.extend_from_slice(&(input.len() as u32).to_be_bytes());
        frame.extend_from_slice(command);
        frame.extend_from_slice(input);
        frame.extend_from_slice(b"ignored transport data");
        let (actual_command, actual_input) = read_frame(frame.as_slice()).unwrap();
        assert_eq!(actual_command, "cat");
        assert_eq!(actual_input, input);
        assert!(read_frame([0xff; 8].as_slice()).is_err());
    }
}
