#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Daemon,
    Trigger { reverse: bool },
}

pub fn parse_args<I, S>(args: I) -> Mode
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args = args.into_iter().map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
    let daemon_mode = args.iter().any(|arg| arg == "daemon");
    let trigger_mode = args.iter().any(|arg| arg == "trigger");
    let reverse = args.iter().any(|arg| arg == "--reverse");

    if trigger_mode {
        return Mode::Trigger { reverse };
    }

    if daemon_mode {
        return Mode::Daemon;
    }

    if args.len() > 1 {
        return Mode::Trigger { reverse };
    }

    Mode::Daemon
}

pub fn trigger_command(reverse: bool) -> &'static str {
    if reverse {
        "SHOW REVERSE\n"
    } else {
        "SHOW FORWARD\n"
    }
}

