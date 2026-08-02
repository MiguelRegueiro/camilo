use std::{sync::mpsc, thread};

use crossterm::event::{self, Event, KeyCode, MouseButton, MouseEventKind};

use super::layout::{GalleryLayout, UiLayout};

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
    Previous,
    Next,
    Close,
    Click { x: u16, y: u16 },
    Resize,
}

pub(crate) fn spawn_input_thread() -> mpsc::Receiver<InputEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(Event::Key(key)) => {
                    let event = match key.code {
                        KeyCode::Char('q') => Some(InputEvent::Quit),
                        KeyCode::Left => Some(InputEvent::Previous),
                        KeyCode::Right => Some(InputEvent::Next),
                        KeyCode::Esc => Some(InputEvent::Close),
                        _ => None,
                    };
                    if let Some(event) = event
                        && tx.send(event).is_err()
                    {
                        break;
                    }
                    if event == Some(InputEvent::Quit) {
                        break;
                    }
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
    pub(crate) open_gallery_requested: &'a mut bool,
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
            InputEvent::Previous | InputEvent::Next | InputEvent::Close => {}
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
                } else if !targets.controls_locked
                    && layout
                        .and_then(|layout| layout.thumbnail_button)
                        .is_some_and(|thumbnail| thumbnail.contains(x, y))
                {
                    *targets.open_gallery_requested = true;
                }
            }
        }
    }
    should_quit
}

pub(crate) struct GalleryInputTargets<'a> {
    pub(crate) navigation: &'a mut isize,
    pub(crate) close_requested: &'a mut bool,
    pub(crate) layout_dirty: &'a mut bool,
}

pub(crate) fn drain_gallery_input_events(
    rx: &mpsc::Receiver<InputEvent>,
    layout: Option<GalleryLayout>,
    targets: GalleryInputTargets<'_>,
) -> bool {
    let mut should_quit = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            InputEvent::Quit => should_quit = true,
            InputEvent::Previous => *targets.navigation -= 1,
            InputEvent::Next => *targets.navigation += 1,
            InputEvent::Close => *targets.close_requested = true,
            InputEvent::Resize => *targets.layout_dirty = true,
            InputEvent::Click { x, y } => {
                if layout
                    .and_then(|layout| layout.close_button)
                    .is_some_and(|button| button.contains(x, y))
                {
                    *targets.close_requested = true;
                } else if layout
                    .and_then(|layout| layout.previous_button)
                    .is_some_and(|button| button.contains(x, y))
                {
                    *targets.navigation -= 1;
                } else if layout
                    .and_then(|layout| layout.next_button)
                    .is_some_and(|button| button.contains(x, y))
                {
                    *targets.navigation += 1;
                }
            }
        }
    }
    should_quit
}

#[cfg(test)]
mod tests {
    use super::super::layout::{GalleryLayout, ImageArea, Rect, UiLayout};
    use super::*;

    #[test]
    fn mode_toggle_click_changes_mode_and_marks_chrome_dirty() {
        let (tx, rx) = mpsc::channel();
        let mut mode = CaptureMode::Photo;
        let mut self_timer = SelfTimer::Off;
        let mut capture_requested = false;
        let mut open_gallery_requested = false;
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
            thumbnail_button: None,
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
                open_gallery_requested: &mut open_gallery_requested,
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
        let mut open_gallery_requested = false;
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
            thumbnail_button: None,
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
                open_gallery_requested: &mut open_gallery_requested,
                chrome_dirty: &mut chrome_dirty,
                layout_dirty: &mut layout_dirty,
            },
        ));
        assert_eq!(self_timer, SelfTimer::Seconds3);
        assert!(!capture_requested);
        assert!(chrome_dirty);
    }

    #[test]
    fn thumbnail_click_requests_gallery() {
        let (tx, rx) = mpsc::channel();
        let mut mode = CaptureMode::Photo;
        let mut self_timer = SelfTimer::Off;
        let mut capture_requested = false;
        let mut open_gallery_requested = false;
        let mut chrome_dirty = false;
        let mut layout_dirty = false;
        let thumbnail = Rect {
            x: 30,
            y: 16,
            cols: 8,
            rows: 5,
        };
        let layout = UiLayout {
            preview_area: ImageArea {
                x: 0,
                y: 0,
                cols: 30,
                rows: 24,
            },
            sidebar: Rect {
                x: 30,
                y: 0,
                cols: 10,
                rows: 24,
            },
            capture_button: None,
            shutter_area: None,
            mode_toggle: None,
            mode_pill_area: None,
            self_timer_toggle: None,
            self_timer_area: None,
            countdown_area: None,
            recording_timer_area: None,
            no_mic_area: None,
            thumbnail_area: None,
            thumbnail_button: Some(thumbnail),
        };
        tx.send(InputEvent::Click { x: 34, y: 18 }).unwrap();

        assert!(!drain_input_events(
            &rx,
            Some(layout),
            InputTargets {
                capture_mode: &mut mode,
                self_timer: &mut self_timer,
                controls_locked: false,
                capture_requested: &mut capture_requested,
                open_gallery_requested: &mut open_gallery_requested,
                chrome_dirty: &mut chrome_dirty,
                layout_dirty: &mut layout_dirty,
            },
        ));
        assert!(open_gallery_requested);
        assert!(!capture_requested);
    }

    #[test]
    fn gallery_input_combines_navigation_and_close_actions() {
        let (tx, rx) = mpsc::channel();
        let previous = Rect {
            x: 1,
            y: 8,
            cols: 5,
            rows: 3,
        };
        let next = Rect {
            x: 34,
            y: 8,
            cols: 5,
            rows: 3,
        };
        let close = Rect {
            x: 34,
            y: 1,
            cols: 5,
            rows: 3,
        };
        let layout = GalleryLayout {
            image_area: ImageArea {
                x: 0,
                y: 0,
                cols: 40,
                rows: 20,
            },
            previous_button: Some(previous),
            previous_area: None,
            next_button: Some(next),
            next_area: None,
            close_button: Some(close),
            close_area: None,
            counter_area: None,
        };
        let mut navigation = 0;
        let mut close_requested = false;
        let mut layout_dirty = false;
        tx.send(InputEvent::Previous).unwrap();
        tx.send(InputEvent::Click { x: 36, y: 9 }).unwrap();
        tx.send(InputEvent::Click { x: 36, y: 2 }).unwrap();
        tx.send(InputEvent::Resize).unwrap();

        assert!(!drain_gallery_input_events(
            &rx,
            Some(layout),
            GalleryInputTargets {
                navigation: &mut navigation,
                close_requested: &mut close_requested,
                layout_dirty: &mut layout_dirty,
            },
        ));
        assert_eq!(navigation, 0);
        assert!(close_requested);
        assert!(layout_dirty);
    }
}
