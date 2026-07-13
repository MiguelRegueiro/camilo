use std::{sync::mpsc, thread};

use crossterm::event::{self, Event, KeyCode, MouseButton, MouseEventKind};

use super::layout::UiLayout;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureMode {
    Photo,
    Video,
}

impl CaptureMode {
    pub(crate) fn toggle(&mut self) {
        *self = match self {
            Self::Photo => Self::Video,
            Self::Video => Self::Photo,
        };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputEvent {
    Quit,
    Click { x: u16, y: u16 },
}

pub(crate) fn spawn_input_thread() -> mpsc::Receiver<InputEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(Event::Key(key)) if key.code == KeyCode::Char('q') => {
                    let _ = tx.send(InputEvent::Quit);
                    break;
                }
                Ok(Event::Mouse(mouse))
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
                {
                    if tx
                        .send(InputEvent::Click {
                            x: mouse.column,
                            y: mouse.row,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = tx.send(InputEvent::Quit);
                    break;
                }
            }
        }
    });
    rx
}

pub(crate) fn drain_input_events(
    rx: &mpsc::Receiver<InputEvent>,
    layout: Option<UiLayout>,
    capture_mode: &mut CaptureMode,
    mode_locked: bool,
    capture_requested: &mut bool,
    chrome_dirty: &mut bool,
) -> bool {
    let mut should_quit = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            InputEvent::Quit => should_quit = true,
            InputEvent::Click { x, y } => {
                if layout
                    .and_then(|layout| layout.capture_button)
                    .is_some_and(|button| button.contains(x, y))
                {
                    *capture_requested = true;
                } else if !mode_locked
                    && layout
                        .and_then(|layout| layout.mode_toggle)
                        .is_some_and(|toggle| toggle.contains(x, y))
                {
                    capture_mode.toggle();
                    *chrome_dirty = true;
                }
            }
        }
    }
    should_quit
}

#[cfg(test)]
mod tests {
    use super::super::layout::{ImageArea, Rect, UiLayout};
    use super::*;

    #[test]
    fn mode_toggle_click_changes_mode_and_marks_chrome_dirty() {
        let (tx, rx) = mpsc::channel();
        let mut mode = CaptureMode::Photo;
        let mut capture_requested = false;
        let mut chrome_dirty = false;
        let layout = UiLayout {
            preview_area: ImageArea {
                x: 0,
                y: 0,
                cols: 20,
                rows: 20,
            },
            sidebar: Rect {
                x: 20,
                y: 0,
                cols: 0,
                rows: 20,
            },
            capture_button: None,
            shutter_area: None,
            mode_toggle: Some(Rect {
                x: 1,
                y: 1,
                cols: 6,
                rows: 8,
            }),
            mode_pill_area: None,
            recording_timer_area: None,
            no_mic_area: None,
            thumbnail_area: None,
        };

        tx.send(InputEvent::Click { x: 3, y: 3 }).unwrap();

        assert!(!drain_input_events(
            &rx,
            Some(layout),
            &mut mode,
            false,
            &mut capture_requested,
            &mut chrome_dirty,
        ));
        assert_eq!(mode, CaptureMode::Video);
        assert!(!capture_requested);
        assert!(chrome_dirty);
    }
}
