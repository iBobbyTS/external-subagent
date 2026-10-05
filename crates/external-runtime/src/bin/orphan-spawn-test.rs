use std::io::{BufRead, Write};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("__orphan-spawn") {
        external_runtime::orphan_spawn_main();
    }
    match args.get(1).map(String::as_str) {
        Some("__fixture") => {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            let keys = [
                "HOME",
                "PATH",
                "OPENAI_API_KEY",
                "ES_TEST_OVERRIDE",
                "ES_TEST_EMPTY",
                "ES_TEST_POLICY",
            ];
            let env: std::collections::BTreeMap<_, _> = keys
                .into_iter()
                .map(|key| (key, std::env::var(key).ok()))
                .collect();
            let fds: Vec<i32> = (3..256)
                .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0)
                .collect();
            println!(
                "{}",
                serde_json::json!({"event":"fixture", "env":env, "args":&args[2..], "cwd":std::env::current_dir().unwrap(), "fds":fds})
            );
        }
        Some("__terminal") => {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            println!("{}", serde_json::json!({"event":"terminal"}));
            eprint!("{}TAIL", "x".repeat(20_000));
            std::process::exit(args.get(2).unwrap().parse().unwrap());
        }
        Some("__ignore_term") => {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            println!("{}", serde_json::json!({"event":"ready"}));
            std::io::stdout().flush().unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        Some("__term_exit") => {
            extern "C" fn term_exit(_: libc::c_int) {
                unsafe { libc::_exit(23) }
            }
            unsafe {
                libc::signal(libc::SIGTERM, term_exit as *const () as libc::sighandler_t);
            }
            println!("{}", serde_json::json!({"event":"ready"}));
            std::io::stdout().flush().unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        Some("__descendant") => {
            let child = unsafe { libc::fork() };
            assert!(child >= 0);
            if child == 0 {
                unsafe {
                    libc::signal(libc::SIGTERM, libc::SIG_IGN);
                    for fd in 0..=2 {
                        libc::close(fd);
                    }
                    loop {
                        libc::pause();
                    }
                }
            }
            println!("{}", serde_json::json!({"event":"descendant", "pid":child}));
            std::io::stdout().flush().unwrap();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            std::process::exit(7);
        }
        Some("__linger") => {
            // Deliberately retain the runtime across daemon failure/EOF to make
            // unauthorized cleanup observable. Integration tests reap it.
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        _ => std::process::exit(64),
    }
}
