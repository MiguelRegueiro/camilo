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
pub(crate) enum SelfTimer {
    Off,
    Seconds3,
    Seconds5,
    Seconds10,
}

impl SelfTimer {
    pub(crate) fn cycle(&mut self) {
        *self = match self {
            Self::Off => Self::Seconds3,
            Self::Seconds3 => Self::Seconds5,
            Self::Seconds5 => Self::Seconds10,
            Self::Seconds10 => Self::Off,
        };
    }

    pub(crate) fn seconds(self) -> Option<u64> {
        match self {
            Self::Off => None,
            Self::Seconds3 => Some(3),
            Self::Seconds5 => Some(5),
            Self::Seconds10 => Some(10),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputEvent {
    Quit,
    Click { x: u16, y: u16 },
    Resize,
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
                Ok(Event::Resize(_, _)) => {
                    if tx.send(InputEvent::Resize).is_err() {
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

pub(crate) struct InputTargets<'a> {
    pub(crate) capture_mode: &'a mut CaptureMode,
    pub(crate) self_timer: &'a mut SelfTimer,
    pub(crate) controls_locked: bool,
    pub(crate) capture_requested: &'a mut bool,
    pub(crate) chrome_dirty: &'a mut bool,
    pub(crate) layout_dirty: &'a mut bool,
}

pub(crate) fn drain_input_events(
    rx: &mpsc::Receiver<InputEvent>,
    layout: Option<UiLayout>,
    targets: InputTargets<'_>,
) -> bool {
    let mut should_quit = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            InputEvent::Quit => should_quit = true,
            InputEvent::Resize => *targets.layout_dirty = true,
            InputEvent::Click { x, y } => {
                if layout
                    .and_then(|layout| layout.capture_button)
                    .is_some_and(|button| button.contains(x, y))
                {
                    *targets.capture_requested = true;
                } else if !targets.controls_locked
                    && layout
                        .and_then(|layout| layout.mode_toggle)
                        .is_some_and(|toggle| toggle.contains(x, y))
                {
                    targets.capture_mode.toggle();
                    *targets.chrome_dirty = true;
                } else if !targets.controls_locked
                    && layout
                        .and_then(|layout| layout.self_timer_toggle)
                        .is_some_and(|toggle| toggle.contains(x, y))
                {
                    targets.self_timer.cycle();
                    *targets.chrome_dirty = true;
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
        let mut self_timer = SelfTimer::Off;
        let mut capture_requested = false;
        let mut chrome_dirty = false;
        let mut layout_dirty = false;
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
            self_timer_toggle: None,
            self_timer_area: None,
            countdown_area: None,
            recording_timer_area: None,
            no_mic_area: None,
            thumbnail_area: None,
        };

        tx.send(InputEvent::Click { x: 3, y: 3 }).unwrap();

        assert!(!drain_input_events(
            &rx,
            Some(layout),
            InputTargets {
                capture_mode: &mut mode,
                self_timer: &mut self_timer,
                controls_locked: false,
                capture_requested: &mut capture_requested,
                chrome_dirty: &mut chrome_dirty,
                layout_dirty: &mut layout_dirty,
            },
        ));
        assert_eq!(mode, CaptureMode::Video);
        assert!(!capture_requested);
        assert!(chrome_dirty);
    }

    #[test]
    fn timer_click_cycles_timer_and_marks_chrome_dirty() {
        let (tx, rx) = mpsc::channel();
        let mut mode = CaptureMode::Photo;
        let mut self_timer = SelfTimer::Off;
        let mut capture_requested = false;
        let mut chrome_dirty = false;
        let mut layout_dirty = false;
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
            mode_toggle: None,
            mode_pill_area: None,
            self_timer_toggle: Some(Rect {
                x: 1,
                y: 7,
                cols: 6,
                rows: 4,
            }),
            self_timer_area: None,
            countdown_area: None,
            recording_timer_area: None,
            no_mic_area: None,
            thumbnail_area: None,
        };

        tx.send(InputEvent::Click { x: 3, y: 8 }).unwrap();

        assert!(!drain_input_events(
            &rx,
            Some(layout),
            InputTargets {
                capture_mode: &mut mode,
                self_timer: &mut self_timer,
                controls_locked: false,
                capture_requested: &mut capture_requested,
                chrome_dirty: &mut chrome_dirty,
                layout_dirty: &mut layout_dirty,
            },
        ));
        assert_eq!(self_timer, SelfTimer::Seconds3);
        assert!(!capture_requested);
        assert!(chrome_dirty);
    }
}
