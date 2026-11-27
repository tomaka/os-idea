use std::{io, process, thread};

/// Runs the given Cargo command, capturing its stdout and stderr and passing each line through
/// `multi`. Each line written to stderr is also shown as the message of `bar`.
///
/// Returns the exit status of the process.
///
/// # Panic
///
/// Panics if spawning the process or waiting for it fails.
pub fn run(
    multi: &indicatif::MultiProgress,
    bar: &indicatif::ProgressBar,
    command: &mut process::Command,
) -> process::ExitStatus {
    // TODO: actually parse the messages from cargo and reformat them
    let mut child = command
        .args(["--color", "always"])
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run cargo: {err}"));

    let Some(stdout) = child.stdout.take() else {
        unreachable!()
    };
    let Some(stderr) = child.stderr.take() else {
        unreachable!()
    };

    thread::scope(|scope| {
        scope.spawn(|| {
            for line in io::BufRead::lines(io::BufReader::new(stdout)) {
                let Ok(line) = line else { break };
                let _ = multi.println(line);
            }
        });

        for line in io::BufRead::lines(io::BufReader::new(stderr)) {
            let Ok(line) = line else { break };
            bar.set_message(line.trim().to_owned());
            let _ = multi.println(line);
        }
    });

    child
        .wait()
        .unwrap_or_else(|err| panic!("failed to wait for cargo: {err}"))
}
