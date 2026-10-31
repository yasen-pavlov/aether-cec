use crate::config::Result;
use aether_logic::console::{ConsoleCommand, MAX_LINE};
use std::{
    io::{Read, Write},
    sync::mpsc::{self, Receiver},
};

pub fn spawn() -> Result<Receiver<ConsoleCommand>> {
    crate::ffi::install_console_uart()?;
    let (sender, receiver) = mpsc::sync_channel(4);
    std::thread::Builder::new()
        .name("aether-console".into())
        // 6 KiB overflowed into the adjacent channel allocation on the bench
        // (intermittent LoadProhibited in the channel's waker mutex on both ends).
        .stack_size(12288)
        .spawn(move || {
            let mut input = std::io::stdin();
            let mut line = Vec::with_capacity(MAX_LINE);
            let mut oversized = false;
            let mut byte = [0];
            prompt();
            loop {
                match input.read(&mut byte) {
                    Ok(0) | Err(_) => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    }
                    Ok(_) => {}
                }
                match byte[0] {
                    b'\r' => {}
                    b'\n' => {
                        if oversized {
                            println!("line_too_long");
                        } else if !line.is_empty() {
                            match std::str::from_utf8(&line)
                                .map_err(|_| "invalid_utf8")
                                .and_then(aether_logic::console::parse)
                            {
                                Ok(command) => {
                                    if sender.send(command).is_err() {
                                        return;
                                    }
                                }
                                Err(reason) => println!("{reason}"),
                            }
                        }
                        line.fill(0);
                        line.clear();
                        oversized = false;
                        prompt();
                    }
                    8 | 127 if !oversized => {
                        line.pop();
                    }
                    _ if !oversized && line.len() < MAX_LINE => line.push(byte[0]),
                    _ => oversized = true,
                }
            }
        })?;
    Ok(receiver)
}
fn prompt() {
    print!("aether> ");
    let _ = std::io::stdout().flush();
}
