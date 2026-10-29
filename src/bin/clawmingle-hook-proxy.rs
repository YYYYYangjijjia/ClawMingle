//! Shell-shaped, IPC-only entry point for native Codex command hooks.
//! Bind this executable at /bin/sh in the authority. It is deliberately not a
//! shell: no command expansion, child processes, filesystem reads, or env copies.

use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    process::ExitCode,
    time::Duration,
};

const SOCKET: &str = "/run/clawmingle-hooks/control.sock";
const INPUT_LIMIT: usize = 1024 * 1024;
const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(_) => {
            eprintln!("ClawMingle hook transport failed");
            ExitCode::FAILURE
        }
    }
}

fn token(arguments: &[String]) -> io::Result<&str> {
    if arguments.len() != 3 || arguments[1] != "-lc" {
        return Err(io::Error::other("unsupported invocation"));
    }
    let token = arguments[2]
        .strip_prefix("clawmingle-hook:")
        .filter(|value| value.len() == 32 && value.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| io::Error::other("invalid binding"))?;
    Ok(token)
}

fn run() -> io::Result<u8> {
    let arguments: Vec<_> = std::env::args().collect();
    let binding = token(&arguments)?;
    let mut input = Vec::new();
    io::stdin()
        .take((INPUT_LIMIT + 1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > INPUT_LIMIT {
        return Err(io::Error::other("input limit"));
    }
    let mut stream = UnixStream::connect(SOCKET)?;
    stream.set_read_timeout(Some(Duration::from_secs(610)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(b"CMHOOK01")?;
    stream.write_all(binding.as_bytes())?;
    stream.write_all(&(input.len() as u32).to_be_bytes())?;
    stream.write_all(&input)?;
    // Keep the write half open: EOF here means native timeout/cancellation.
    let mut header = [0_u8; 20];
    stream.read_exact(&mut header)?;
    if &header[..8] != b"CMHOOR01" {
        return Err(io::Error::other("invalid response"));
    }
    let code = i32::from_be_bytes(header[8..12].try_into().unwrap());
    let stdout_size = u32::from_be_bytes(header[12..16].try_into().unwrap()) as usize;
    let stderr_size = u32::from_be_bytes(header[16..20].try_into().unwrap()) as usize;
    if stdout_size.saturating_add(stderr_size) > OUTPUT_LIMIT {
        return Err(io::Error::other("output limit"));
    }
    let mut output = vec![0; stdout_size + stderr_size];
    stream.read_exact(&mut output)?;
    io::stdout().write_all(&output[..stdout_size])?;
    io::stderr().write_all(&output[stdout_size..])?;
    Ok(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_shell_syntax_and_non_native_invocations() {
        for command in [
            "echo secret",
            "clawmingle-hook:abc",
            "clawmingle-hook:00000000000000000000000000000000; id",
            "$(id)",
        ] {
            assert!(token(&["sh".into(), "-lc".into(), command.into()]).is_err());
        }
        assert!(
            token(&[
                "sh".into(),
                "-c".into(),
                "clawmingle-hook:00000000000000000000000000000000".into()
            ])
            .is_err()
        );
        assert!(
            token(&[
                "sh".into(),
                "-lc".into(),
                "clawmingle-hook:00000000000000000000000000000000".into()
            ])
            .is_ok()
        );
    }
}
