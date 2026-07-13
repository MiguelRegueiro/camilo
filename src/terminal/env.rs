use std::{
    env,
    ffi::OsStr,
    process::{Command, Stdio},
};

pub(crate) fn looks_like_kitty() -> bool {
    env::var("TERM")
        .map(|term| term.to_ascii_lowercase().contains("kitty"))
        .unwrap_or(false)
        || env::var_os("KITTY_WINDOW_ID").is_some()
        || env::var("TERM_PROGRAM")
            .map(|term| term.eq_ignore_ascii_case("kitty"))
            .unwrap_or(false)
}

pub(crate) fn inside_tmux() -> bool {
    env::var_os("TMUX").is_some()
}

pub(crate) fn enable_tmux_passthrough() {
    let mut args = vec!["set-option".into(), "-p".into(), "-q".into()];
    if let Some(pane) = env::var_os("TMUX_PANE")
        && !pane.is_empty()
    {
        args.push("-t".into());
        args.push(pane);
    }
    args.push("allow-passthrough".into());
    args.push("on".into());

    let _ = Command::new("tmux")
        .args(args.iter().map(OsStr::new))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
