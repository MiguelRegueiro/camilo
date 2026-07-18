use std::{
    env,
    ffi::OsStr,
    process::{Command, Stdio},
    sync::atomic::{AtomicU16, Ordering},
};

static TMUX_PANE_ORIGIN_X: AtomicU16 = AtomicU16::new(0);
static TMUX_PANE_ORIGIN_Y: AtomicU16 = AtomicU16::new(0);

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
    refresh_tmux_pane_origin();
}

pub(crate) fn refresh_tmux_pane_origin() {
    let mut command = Command::new("tmux");
    command.args(["display-message", "-p"]);
    if let Some(pane) = env::var_os("TMUX_PANE")
        && !pane.is_empty()
    {
        command.arg("-t").arg(pane);
    }
    command.arg(
        "#{pane_left},#{pane_top},#{window_offset_x},#{window_offset_y},#{status},#{status-position}",
    );

    let Ok(output) = command.stdin(Stdio::null()).stderr(Stdio::null()).output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let Ok(text) = str::from_utf8(&output.stdout) else {
        return;
    };
    let Some((origin_x, origin_y)) = parse_tmux_pane_origin(text) else {
        return;
    };

    TMUX_PANE_ORIGIN_X.store(origin_x, Ordering::Relaxed);
    TMUX_PANE_ORIGIN_Y.store(origin_y, Ordering::Relaxed);
}

fn parse_tmux_pane_origin(text: &str) -> Option<(u16, u16)> {
    let mut values = text.trim().split(',');
    let (
        Some(left),
        Some(top),
        Some(offset_x),
        Some(offset_y),
        Some(status),
        Some(status_position),
    ) = (
        values.next(),
        values.next(),
        values.next(),
        values.next(),
        values.next(),
        values.next(),
    )
    else {
        return None;
    };
    let (Ok(left), Ok(top)) = (left.parse::<u16>(), top.parse::<u16>()) else {
        return None;
    };
    let offset_x = offset_x.parse::<u16>().unwrap_or(0);
    let offset_y = offset_y.parse::<u16>().unwrap_or(0);
    let status_rows = if status_position == "top" {
        match status {
            "off" => 0,
            "on" => 1,
            rows => rows.parse::<u16>().unwrap_or(1),
        }
    } else {
        0
    };

    Some((
        left.saturating_sub(offset_x),
        top.saturating_sub(offset_y).saturating_add(status_rows),
    ))
}

pub(crate) fn tmux_pane_origin() -> (u16, u16) {
    (
        TMUX_PANE_ORIGIN_X.load(Ordering::Relaxed),
        TMUX_PANE_ORIGIN_Y.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_pane_origin_accounts_for_viewport_and_top_status() {
        assert_eq!(parse_tmux_pane_origin("41,5,3,2,3,top\n"), Some((38, 6)));
    }

    #[test]
    fn tmux_pane_origin_ignores_bottom_status() {
        assert_eq!(parse_tmux_pane_origin("41,5,,,on,bottom\n"), Some((41, 5)));
    }
}
